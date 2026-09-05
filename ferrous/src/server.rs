use std::collections::VecDeque;
use std::path::{Component, Path, PathBuf};

use ferrum_core::{
    modified_secs, normalize_path, Capability, FerrousRequest, FerrousResponse, FileEntry, FileMeta,
    SearchMatch, MAX_TRANSFER_CHUNK,
};
use tokio::fs;

/// Search stops after this many hits so one broad query cannot pin the agent
/// walking a huge tree, or push a multi-megabyte reply through the channel.
const MAX_SEARCH_MATCHES: usize = 500;
/// And after this many directory entries examined, hit or not.
const MAX_SEARCH_VISITS: usize = 50_000;
/// Files above this size are skipped by content search; they are almost
/// never the text the user is looking for and would dominate the walk.
const MAX_CONTENT_SEARCH_BYTES: u64 = 2 * 1024 * 1024;

pub struct FerrousServer;

fn forbidden() -> FerrousResponse {
    FerrousResponse::Forbidden { message: "Access denied by capability policy".into() }
}

fn read_only_or_forbidden() -> FerrousResponse {
    FerrousResponse::Forbidden { message: "Permission denied or read-only mode".into() }
}

fn error(e: impl ToString) -> FerrousResponse {
    FerrousResponse::Error { message: e.to_string() }
}

/// `canonicalize` on Windows returns `\\?\C:\...`; nothing downstream wants
/// that prefix and it breaks prefix comparison against configured roots.
pub fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => path,
    }
}

/// The real location `path` refers to: its deepest existing ancestor is
/// canonicalized (which follows every symlink on the way), and any
/// not-yet-existing tail is appended unchanged. Works for a file about to
/// be created as well as for one that exists.
fn realpath(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(canon) = std::fs::canonicalize(&existing) {
            let mut out = strip_verbatim(canon);
            for seg in tail.iter().rev() {
                out.push(seg);
            }
            return out;
        }
        match (existing.file_name().map(|s| s.to_os_string()), existing.parent()) {
            (Some(name), Some(parent)) if !parent.as_os_str().is_empty() => {
                tail.push(name);
                existing = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Where a request's path actually points, if the capability allows it.
///
/// Two checks, both required:
///
/// 1. Lexical: `.` and `..` are resolved and the result must sit under an
///    allowed root. This is what stops `/srv/data/../../etc/shadow`.
/// 2. Physical: the path is resolved through the real filesystem so a
///    symlink inside an allowed root that points outside it is caught too.
///    A grant on `/srv/data` must not become a grant on `/` because someone
///    dropped `ln -s / escape` in there.
///
/// On success returns both the normalized request path (what the client
/// navigates by, echoed back in listings) and the real path to operate on.
pub fn resolve_real(cap: &Capability, raw: &str) -> Result<(String, PathBuf), FerrousResponse> {
    let lexical = normalize_path(raw);
    if !cap.is_path_allowed(&lexical) {
        return Err(forbidden());
    }

    let real = realpath(Path::new(&lexical));
    // Reject anything that still carries a `..` (a relative path on a
    // filesystem we could not canonicalize) rather than guess.
    if real.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(forbidden());
    }
    if !cap.is_path_allowed(&real.to_string_lossy()) {
        tracing::warn!(
            "Refusing {:?}: resolves to {:?}, outside every allowed path (symlink escape?)",
            raw,
            real
        );
        return Err(forbidden());
    }

    Ok((lexical, real))
}

/// Up to `len` bytes of `path` from `offset`, and whether the file ended
/// within them. A read returning fewer bytes than asked for is what marks
/// the end: the client keeps going until it sees `eof`.
async fn read_chunk(path: &Path, offset: u64, len: usize) -> std::io::Result<(Vec<u8>, bool)> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file = fs::File::open(path).await?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut buf = vec![0u8; len];
    let mut filled = 0;
    while filled < len {
        let n = file.read(&mut buf[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buf.truncate(filled);
    Ok((buf, filled < len))
}

async fn write_chunk(path: &Path, offset: u64, data: &[u8], truncate: bool) -> std::io::Result<()> {
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(truncate)
        .open(path)
        .await?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    file.write_all(data).await?;
    file.flush().await
}

/// Validates the first message of a session. `Ok` carries the client's
/// release for the log; `Err` carries the message to send back before
/// closing, phrased so whoever reads it knows which side to upgrade.
pub fn check_hello(first: &FerrousRequest) -> Result<String, String> {
    match first {
        FerrousRequest::Hello { protocol_version, version } if *protocol_version == ferrum_core::PROTOCOL_VERSION => {
            Ok(version.clone())
        }
        FerrousRequest::Hello { protocol_version, version } => Err(format!(
            "Ferrite v{} speaks protocol {}, this agent is v{} and speaks protocol {}. \
             Upgrade both twins to the same release.",
            version, protocol_version, ferrum_core::VERSION, ferrum_core::PROTOCOL_VERSION
        )),
        _ => Err(format!(
            "Expected a version exchange first; the client predates v0.2.0. This agent is v{} \
             (protocol {}). Upgrade both twins to the same release.",
            ferrum_core::VERSION, ferrum_core::PROTOCOL_VERSION
        )),
    }
}

pub fn hello_response() -> FerrousResponse {
    FerrousResponse::Hello {
        protocol_version: ferrum_core::PROTOCOL_VERSION,
        version: ferrum_core::VERSION.to_string(),
    }
}

fn child_path(parent_lexical: &str, name: &str) -> String {
    if parent_lexical == "/" {
        format!("/{}", name)
    } else {
        format!("{}/{}", parent_lexical.trim_end_matches('/'), name)
    }
}

impl FerrousServer {
    pub fn new() -> Self {
        Self
    }

    pub async fn handle_request(&self, req: FerrousRequest, capability: &Capability) -> FerrousResponse {
        match req {
            FerrousRequest::ListDir { path } => {
                let (lexical, real) = match resolve_real(capability, &path) {
                    Ok(v) => v,
                    Err(resp) => return resp,
                };
                match self.list_dir(&lexical, &real).await {
                    Ok(entries) => FerrousResponse::ListDir { entries },
                    Err(e) => error(e),
                }
            }
            FerrousRequest::ReadChunk { path, offset, len } => {
                let (_, real) = match resolve_real(capability, &path) {
                    Ok(v) => v,
                    Err(resp) => return resp,
                };
                if len as usize > MAX_TRANSFER_CHUNK {
                    return error(format!("chunk of {} bytes exceeds the {} byte limit", len, MAX_TRANSFER_CHUNK));
                }
                match read_chunk(&real, offset, len as usize).await {
                    Ok((data, eof)) => FerrousResponse::Chunk { data, eof },
                    Err(e) => error(e),
                }
            }
            FerrousRequest::WriteChunk { path, offset, data, truncate } => {
                if capability.read_only {
                    return read_only_or_forbidden();
                }
                let (_, real) = match resolve_real(capability, &path) {
                    Ok(v) => v,
                    Err(_) => return read_only_or_forbidden(),
                };
                if data.len() > MAX_TRANSFER_CHUNK {
                    return error(format!("chunk of {} bytes exceeds the {} byte limit", data.len(), MAX_TRANSFER_CHUNK));
                }
                if let Some(parent) = real.parent() {
                    let _ = fs::create_dir_all(parent).await;
                }
                match write_chunk(&real, offset, &data, truncate).await {
                    Ok(()) => FerrousResponse::Success,
                    Err(e) => error(e),
                }
            }
            FerrousRequest::Stat { path } => {
                let (_, real) = match resolve_real(capability, &path) {
                    Ok(v) => v,
                    Err(resp) => return resp,
                };
                match fs::metadata(&real).await {
                    Ok(m) => FerrousResponse::Stat { meta: FileMeta::from_metadata(&m) },
                    Err(e) => error(e),
                }
            }
            FerrousRequest::Delete { path } => {
                if capability.read_only {
                    return read_only_or_forbidden();
                }
                let (_, real) = match resolve_real(capability, &path) {
                    Ok(v) => v,
                    Err(_) => return read_only_or_forbidden(),
                };
                // symlink_metadata so deleting a symlink removes the link,
                // never the directory it points at.
                let res = match fs::symlink_metadata(&real).await {
                    Ok(m) if m.is_dir() => fs::remove_dir_all(&real).await,
                    Ok(_) => fs::remove_file(&real).await,
                    Err(e) => Err(e),
                };
                match res {
                    Ok(_) => FerrousResponse::Success,
                    Err(e) => error(e),
                }
            }
            FerrousRequest::Rename { from, to } => {
                if capability.read_only {
                    return read_only_or_forbidden();
                }
                let (_, from_real) = match resolve_real(capability, &from) {
                    Ok(v) => v,
                    Err(_) => return read_only_or_forbidden(),
                };
                let (_, to_real) = match resolve_real(capability, &to) {
                    Ok(v) => v,
                    Err(_) => return read_only_or_forbidden(),
                };
                match fs::rename(&from_real, &to_real).await {
                    Ok(_) => FerrousResponse::Success,
                    Err(e) => error(e),
                }
            }
            FerrousRequest::CreateFile { path } => {
                if capability.read_only {
                    return read_only_or_forbidden();
                }
                let (_, real) = match resolve_real(capability, &path) {
                    Ok(v) => v,
                    Err(_) => return read_only_or_forbidden(),
                };
                // create_new: creating a file that exists must not truncate it.
                match fs::OpenOptions::new().write(true).create_new(true).open(&real).await {
                    Ok(_) => FerrousResponse::Success,
                    Err(e) => error(e),
                }
            }
            FerrousRequest::CreateDir { path } => {
                if capability.read_only {
                    return read_only_or_forbidden();
                }
                let (_, real) = match resolve_real(capability, &path) {
                    Ok(v) => v,
                    Err(_) => return read_only_or_forbidden(),
                };
                match fs::create_dir_all(&real).await {
                    Ok(_) => FerrousResponse::Success,
                    Err(e) => error(e),
                }
            }
            FerrousRequest::Search { path, query, content_search } => {
                let (lexical, real) = match resolve_real(capability, &path) {
                    Ok(v) => v,
                    Err(resp) => return resp,
                };
                match self.search_dir(&lexical, &real, &query, content_search).await {
                    Ok(matches) => FerrousResponse::Search { matches },
                    Err(e) => error(e),
                }
            }
            // Only valid as the first message, which main.rs consumes before
            // handing anything to this function.
            FerrousRequest::Hello { .. } => error("Hello is only accepted as the first message of a session"),
            FerrousRequest::PtyOpen { .. } | FerrousRequest::PtyInput { .. } | FerrousRequest::PtyResize { .. } => {
                if !capability.allow_shell {
                    return FerrousResponse::Forbidden { message: "Shell access disabled by capability policy".into() };
                }
                FerrousResponse::Success
            }
        }
    }

    async fn list_dir(&self, lexical: &str, real: &Path) -> Result<Vec<FileEntry>, String> {
        let mut dir = fs::read_dir(real).await.map_err(|e| e.to_string())?;
        let mut entries = Vec::new();
        while let Ok(Some(entry)) = dir.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            let meta = entry.metadata().await.ok();
            entries.push(FileEntry {
                path: child_path(lexical, &name),
                name,
                is_dir: meta.as_ref().map(|m| m.is_dir()).unwrap_or(false),
                size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                modified: meta.as_ref().and_then(modified_secs),
            });
        }
        entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        Ok(entries)
    }

    /// Recursive, case-insensitive search below `real`, matching names or
    /// (with `content_search`) lines of text files. Symlinked directories are
    /// not descended into, which both avoids cycles and keeps the walk inside
    /// the root that was authorized.
    async fn search_dir(
        &self,
        lexical: &str,
        real: &Path,
        query: &str,
        content_search: bool,
    ) -> Result<Vec<SearchMatch>, String> {
        let needle = query.to_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }

        let mut matches = Vec::new();
        let mut visited = 0usize;
        let mut queue: VecDeque<(String, PathBuf)> = VecDeque::new();
        queue.push_back((lexical.to_string(), real.to_path_buf()));

        while let Some((dir_lexical, dir_real)) = queue.pop_front() {
            let mut dir = match fs::read_dir(&dir_real).await {
                Ok(d) => d,
                Err(e) if dir_real == real => return Err(e.to_string()),
                Err(_) => continue,
            };

            while let Ok(Some(entry)) = dir.next_entry().await {
                visited += 1;
                if matches.len() >= MAX_SEARCH_MATCHES || visited >= MAX_SEARCH_VISITS {
                    return Ok(matches);
                }

                let name = entry.file_name().to_string_lossy().to_string();
                let entry_lexical = child_path(&dir_lexical, &name);
                let Ok(file_type) = entry.file_type().await else { continue };

                if file_type.is_dir() {
                    queue.push_back((entry_lexical, entry.path()));
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }

                if content_search {
                    let Ok(meta) = entry.metadata().await else { continue };
                    if meta.len() > MAX_CONTENT_SEARCH_BYTES {
                        continue;
                    }
                    let Ok(bytes) = fs::read(entry.path()).await else { continue };
                    let Ok(text) = std::str::from_utf8(&bytes) else { continue };
                    for (idx, line) in text.lines().enumerate() {
                        if line.to_lowercase().contains(&needle) {
                            matches.push(SearchMatch {
                                path: entry_lexical.clone(),
                                line_number: Some(idx + 1),
                                snippet: Some(line.trim().chars().take(200).collect()),
                            });
                            if matches.len() >= MAX_SEARCH_MATCHES {
                                return Ok(matches);
                            }
                        }
                    }
                } else if name.to_lowercase().contains(&needle) {
                    matches.push(SearchMatch { path: entry_lexical, line_number: None, snippet: None });
                }
            }
        }

        Ok(matches)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(root: &Path) -> Capability {
        Capability {
            allowed_paths: vec![root.to_string_lossy().to_string()],
            read_only: false,
            allow_shell: false,
            shell: None,
        }
    }

    /// A fresh directory under the system temp dir, removed on drop. Kept
    /// dependency-free so the agent's test build stays as small as the agent.
    struct TempRoot(PathBuf);

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_root(tag: &str) -> TempRoot {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("ferrous-{}-{}-{}", tag, std::process::id(), n));
        std::fs::create_dir_all(&path).unwrap();
        TempRoot(path)
    }

    fn sandbox() -> (TempRoot, PathBuf) {
        let dir = temp_root("server");
        let root = strip_verbatim(std::fs::canonicalize(&dir.0).unwrap());
        (dir, root)
    }

    #[test]
    fn hello_is_accepted_only_for_the_current_protocol() {
        let current = FerrousRequest::Hello { protocol_version: ferrum_core::PROTOCOL_VERSION, version: "x".into() };
        assert_eq!(check_hello(&current).unwrap(), "x");

        let older = FerrousRequest::Hello { protocol_version: ferrum_core::PROTOCOL_VERSION - 1, version: "0.1.1".into() };
        let err = check_hello(&older).unwrap_err();
        assert!(err.contains("0.1.1") && err.contains(ferrum_core::VERSION), "{err}");

        let not_hello = FerrousRequest::ListDir { path: "/".into() };
        assert!(check_hello(&not_hello).unwrap_err().contains("predates"));
    }

    #[test]
    fn resolves_inside_the_root() {
        let (_dir, root) = sandbox();
        let c = cap(&root);
        let inside = root.join("a/b.txt");
        let (_, real) = resolve_real(&c, &inside.to_string_lossy()).unwrap();
        assert_eq!(real, inside);
    }

    #[test]
    fn a_path_that_does_not_exist_yet_still_resolves() {
        let (_dir, root) = sandbox();
        let c = cap(&root);
        let (_, real) = resolve_real(&c, &root.join("new/deeper/file").to_string_lossy()).unwrap();
        assert!(real.starts_with(&root));
    }

    #[test]
    fn rejects_lexical_traversal() {
        let (_dir, root) = sandbox();
        let c = cap(&root);
        let escape = format!("{}/../../etc/passwd", root.to_string_lossy());
        assert!(matches!(resolve_real(&c, &escape), Err(FerrousResponse::Forbidden { .. })));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_symlink_pointing_outside_the_root() {
        let (_dir, root) = sandbox();
        let c = cap(&root);
        let outside = temp_root("outside");
        std::fs::write(outside.0.join("secret"), b"x").unwrap();
        std::os::unix::fs::symlink(&outside.0, root.join("escape")).unwrap();

        let via_link = root.join("escape/secret");
        assert!(matches!(resolve_real(&c, &via_link.to_string_lossy()), Err(FerrousResponse::Forbidden { .. })));
    }

    #[tokio::test]
    async fn search_is_recursive_and_supports_content() {
        let (_dir, root) = sandbox();
        std::fs::create_dir_all(root.join("sub/deeper")).unwrap();
        std::fs::write(root.join("sub/deeper/needle.txt"), "first line\nhas the Needle here\n").unwrap();
        std::fs::write(root.join("other.txt"), "nothing").unwrap();

        let c = cap(&root);
        let server = FerrousServer::new();
        let by_name = server
            .handle_request(
                FerrousRequest::Search {
                    path: root.to_string_lossy().to_string(),
                    query: "needle".into(),
                    content_search: false,
                },
                &c,
            )
            .await;
        match by_name {
            FerrousResponse::Search { matches } => {
                assert_eq!(matches.len(), 1);
                assert!(matches[0].path.ends_with("sub/deeper/needle.txt"), "{}", matches[0].path);
            }
            other => panic!("unexpected {other:?}"),
        }

        let by_content = server
            .handle_request(
                FerrousRequest::Search {
                    path: root.to_string_lossy().to_string(),
                    query: "NEEDLE".into(),
                    content_search: true,
                },
                &c,
            )
            .await;
        match by_content {
            FerrousResponse::Search { matches } => {
                assert_eq!(matches.len(), 1);
                assert_eq!(matches[0].line_number, Some(2));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn listings_carry_timestamps_and_client_facing_paths() {
        let (_dir, root) = sandbox();
        std::fs::write(root.join("f.txt"), b"hi").unwrap();
        let c = cap(&root);
        let resp = FerrousServer::new()
            .handle_request(FerrousRequest::ListDir { path: root.to_string_lossy().to_string() }, &c)
            .await;
        match resp {
            FerrousResponse::ListDir { entries } => {
                assert_eq!(entries.len(), 1);
                assert!(entries[0].modified.is_some());
                assert_eq!(entries[0].path, format!("{}/f.txt", normalize_path(&root.to_string_lossy())));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
