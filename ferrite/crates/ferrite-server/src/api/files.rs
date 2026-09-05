//! Everything that reads or writes through a `RemoteFilesystem` backend.
//! Bodies are streamed in both directions wherever the backend can, so a
//! file's size is bounded by the configured upload limit on the way in and
//! by nothing on the way out.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::{
    body::{Body, Bytes},
    extract::{Multipart, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;
use tower_cookies::Cookies;

use super::{invalid, local_disk_path, safe_file_name, ApiError, AppState, PluginEvent};
use crate::permissions::authorize;
use ferrite_core::{RemoteError, RemoteFilesystem};

/// A single HTTP range response never streams more than this at once; the
/// browser asks again for the rest. Keeps a `Range: bytes=0-` on a huge file
/// from turning into one enormous response body.
const MAX_RANGE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Deserialize)]
pub struct FileQuery {
    pub remote: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct RenamePayload {
    pub remote: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub remote: String,
    pub path: String,
    pub query: String,
    pub content: Option<bool>,
}

fn mime_for(path: &str) -> HeaderValue {
    let mime = mime_guess::from_path(path).first_or_octet_stream().to_string();
    mime.parse().unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"))
}

/// `Content-Disposition` for a download. The plain `filename` is reduced to
/// printable ASCII so no name can smuggle a quote or a newline into the
/// header; the RFC 5987 `filename*` carries the real name for browsers that
/// understand it (all current ones).
fn attachment_disposition(name: &str) -> HeaderValue {
    let ascii: String = name
        .chars()
        .map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' })
        .collect();
    let ascii = if ascii.trim().is_empty() { "download".to_string() } else { ascii };

    let mut encoded = String::with_capacity(name.len() * 3);
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            encoded.push(b as char);
        } else {
            encoded.push_str(&format!("%{:02X}", b));
        }
    }
    let value = format!("attachment; filename=\"{}\"; filename*=UTF-8''{}", ascii, encoded);
    value.parse().unwrap_or_else(|_| HeaderValue::from_static("attachment; filename=\"download\""))
}

fn stream_body(stream: ferrite_core::traits::BoxStream<'static, Result<Bytes, RemoteError>>) -> Body {
    Body::from_stream(stream.map(|res| res.map_err(|e| std::io::Error::other(e.to_string()))))
}

pub async fn list_files(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<FileQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, &query.remote, &query.path, false)?;
    let backend = state.get_backend(&query.remote)?;
    let entries = backend.list_dir(&query.path).await?;
    Ok(Json(entries))
}

/// Parses `bytes=start-end` against a known length into an inclusive range.
fn parse_range(spec: &str, total: u64) -> Option<(u64, u64)> {
    let spec = spec.strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    let (start, end) = if start.is_empty() {
        // Suffix range: the last N bytes.
        let n: u64 = end.parse().ok()?;
        (total.saturating_sub(n), total.saturating_sub(1))
    } else {
        let s: u64 = start.parse().ok()?;
        let e: u64 = if end.is_empty() { total.saturating_sub(1) } else { end.parse().ok()? };
        (s, e)
    };
    if total == 0 || start >= total || end < start {
        return None;
    }
    let end = end.min(total - 1).min(start + MAX_RANGE_BYTES - 1);
    Some((start, end))
}

pub async fn read_file(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    headers: HeaderMap,
    Query(query): Query<FileQuery>,
) -> Result<Response, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, &query.remote, &query.path, false)?;
    let backend = state.get_backend(&query.remote)?;

    let _ = state.event_tx.send(PluginEvent::FileOpened {
        remote: query.remote.clone(),
        path: query.path.clone(),
    });

    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(header::CONTENT_TYPE, mime_for(&query.path));
    resp_headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));

    // Only a backend that lives on this machine's disk may be opened directly
    // here (to honour HTTP range requests). Every other remote streams through
    // its backend; the raw request path is never opened on the server's disk
    // just because a file of that name happens to exist there too.
    if let Some(local) = backend.local_path(&query.path).filter(|p| p.is_file()) {
        let mut file = tokio::fs::File::open(&local)
            .await
            .map_err(|e| RemoteError::NotFound(format!("File not found '{}': {}", query.path, e)))?;
        let total = file.metadata().await.map(|m| m.len()).unwrap_or(0);
        resp_headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));

        if let Some(spec) = headers.get(header::RANGE).and_then(|h| h.to_str().ok()) {
            let Some((start, end)) = parse_range(spec, total) else {
                resp_headers.insert(header::CONTENT_RANGE, format!("bytes */{}", total).parse().unwrap());
                return Ok((StatusCode::RANGE_NOT_SATISFIABLE, resp_headers).into_response());
            };
            let len = end - start + 1;
            file.seek(std::io::SeekFrom::Start(start)).await.map_err(RemoteError::Io)?;
            resp_headers.insert(header::CONTENT_RANGE, format!("bytes {}-{}/{}", start, end, total).parse().unwrap());
            resp_headers.insert(header::CONTENT_LENGTH, len.to_string().parse().unwrap());
            let body = Body::from_stream(ReaderStream::new(file.take(len)));
            return Ok((StatusCode::PARTIAL_CONTENT, resp_headers, body).into_response());
        }

        resp_headers.insert(header::CONTENT_LENGTH, total.to_string().parse().unwrap());
        return Ok((StatusCode::OK, resp_headers, Body::from_stream(ReaderStream::new(file))).into_response());
    }

    let stream = backend.stream_file(&query.path).await?;
    Ok((StatusCode::OK, resp_headers, stream_body(stream)).into_response())
}

pub async fn write_file(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<FileQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, &query.remote, &query.path, true)?;
    let backend = state.get_backend(&query.remote)?;

    if let Some(if_match) = headers.get(header::IF_MATCH).and_then(|h| h.to_str().ok()) {
        if let Ok(existing_content) = backend.read_file(&query.path).await {
            let current_hash = format!("{:x}", Sha256::digest(&existing_content));
            if current_hash != if_match.trim_matches('"') {
                return Err(ApiError(RemoteError::OperationFailed(
                    "Remote file has been modified externally".to_string(),
                )));
            }
        }
    }

    backend.write_file(&query.path, &body).await?;

    let _ = state.event_tx.send(PluginEvent::FileSaved {
        remote: query.remote.clone(),
        path: query.path.clone(),
    });

    Ok(StatusCode::OK)
}

pub async fn create_file(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<FileQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, &query.remote, &query.path, true)?;
    state.get_backend(&query.remote)?.create_file(&query.path).await?;
    Ok(StatusCode::CREATED)
}

pub async fn create_directory(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<FileQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, &query.remote, &query.path, true)?;
    state.get_backend(&query.remote)?.create_dir(&query.path).await?;
    Ok(StatusCode::CREATED)
}

pub async fn delete_entry(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<FileQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, &query.remote, &query.path, true)?;
    state.get_backend(&query.remote)?.delete(&query.path).await?;
    Ok(StatusCode::OK)
}

pub async fn rename_entry(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Json(payload): Json<RenamePayload>,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, &payload.remote, &payload.from, true)?;
    authorize(&user, &payload.remote, &payload.to, true)?;
    state.get_backend(&payload.remote)?.rename(&payload.from, &payload.to).await?;
    Ok(StatusCode::OK)
}

pub async fn copy_entry(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Json(payload): Json<RenamePayload>,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, &payload.remote, &payload.from, false)?;
    authorize(&user, &payload.remote, &payload.to, true)?;
    let backend = state.get_backend(&payload.remote)?;
    // Streamed end to end: a copy never holds the whole file in memory.
    let source = backend.stream_file(&payload.from).await?;
    backend.write_stream(&payload.to, source).await?;
    Ok(StatusCode::OK)
}

pub async fn download_file(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<FileQuery>,
) -> Result<Response, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, &query.remote, &query.path, false)?;
    let backend = state.get_backend(&query.remote)?;
    let stream = backend.stream_file(&query.path).await?;

    let filename = Path::new(&query.path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".to_string());

    let mut response = Response::new(stream_body(stream));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    headers.insert(header::CONTENT_DISPOSITION, attachment_disposition(&filename));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    Ok(response)
}

pub async fn upload_file(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<FileQuery>,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, &query.remote, &query.path, true)?;
    let backend = state.get_backend(&query.remote)?;

    let mut uploaded = 0usize;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| invalid(format!("Malformed multipart upload: {}", e)))?
    {
        // Only the base name of what the browser sent is used. The directory
        // is the authorized `path`; a file name carrying `../` must not be
        // able to steer the write anywhere else.
        let Some(name) = field.file_name().and_then(safe_file_name) else {
            continue;
        };
        let target_path = if query.path.ends_with('/') || query.path.ends_with('\\') {
            format!("{}{}", query.path, name)
        } else {
            format!("{}/{}", query.path, name)
        };
        authorize(&user, &query.remote, &target_path, true)?;

        // The multipart field is streamed into the backend through a channel:
        // the field borrows the request, and the backend wants a 'static
        // stream, so the two meet in the middle without buffering the file.
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, RemoteError>>(4);
        let rx_stream = Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }));
        let backend_for_task = backend.clone();
        let target_for_task = target_path.clone();
        let writer = tokio::spawn(async move { backend_for_task.write_stream(&target_for_task, rx_stream).await });

        let mut pump_error: Option<RemoteError> = None;
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) => {
                    if tx.send(Ok(chunk)).await.is_err() {
                        break; // the writer stopped; its error is reported below
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let err = RemoteError::Io(std::io::Error::other(e.to_string()));
                    pump_error = Some(RemoteError::Io(std::io::Error::other(err.to_string())));
                    let _ = tx.send(Err(err)).await;
                    break;
                }
            }
        }
        drop(tx);
        writer
            .await
            .map_err(|e| RemoteError::OperationFailed(format!("Upload task failed: {}", e)))??;
        if let Some(e) = pump_error {
            return Err(ApiError(e));
        }
        uploaded += 1;
    }

    if uploaded == 0 {
        return Err(invalid("No file in upload"));
    }
    Ok(StatusCode::OK)
}

#[derive(Debug, Deserialize)]
pub struct ZipDownloadPayload {
    pub remote: String,
    pub paths: Vec<String>,
}

/// A file in the OS temp directory that is removed when the value drops.
/// Used to spool an archive: zip needs a seekable writer to finalize entry
/// sizes, so it cannot stream straight to the socket, and a temp file keeps
/// that off the heap however large the archive gets.
struct SpoolFile(PathBuf);

impl SpoolFile {
    fn create() -> std::io::Result<(Self, std::fs::File)> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("ferrite-zip-{}-{}.tmp", std::process::id(), n));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok((Self(path), file))
    }
}

impl Drop for SpoolFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Streams a spool file to the client and deletes it once the response body
/// is dropped, whether the download finished or the client went away.
struct SpoolStream {
    inner: ReaderStream<tokio::fs::File>,
    // Dropped after `inner` (declaration order), so the handle is closed
    // before the file is removed, which Windows insists on.
    _spool: SpoolFile,
}

impl Stream for SpoolStream {
    type Item = std::io::Result<Bytes>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

pub async fn download_zip(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Json(payload): Json<ZipDownloadPayload>,
) -> Result<Response, ApiError> {
    let user = state.current_user(&cookies)?;
    if payload.paths.is_empty() {
        return Err(invalid("No paths to archive"));
    }
    for path in &payload.paths {
        authorize(&user, &payload.remote, path, false)?;
    }
    let backend = state.get_backend(&payload.remote)?;

    let (spool, file) = SpoolFile::create().map_err(RemoteError::Io)?;
    let paths = payload.paths;

    // The zip writer is synchronous; it runs on a blocking thread and pulls
    // each file's chunks from the async backend as it goes.
    let file = tokio::task::spawn_blocking(move || -> Result<std::fs::File, RemoteError> {
        let handle = tokio::runtime::Handle::current();
        let mut zip_writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .large_file(true);
        for path in paths {
            if let Err(err) = handle.block_on(zip_add_path(&*backend, &mut zip_writer, &path, options)) {
                tracing::warn!("Failed to add path '{}' to zip: {}", path, err);
            }
        }
        zip_writer
            .finish()
            .map_err(|e| RemoteError::OperationFailed(format!("Zip finish error: {}", e)))
    })
    .await
    .map_err(|e| RemoteError::OperationFailed(format!("Zip task failed: {}", e)))??;

    let mut file = tokio::fs::File::from_std(file);
    file.seek(std::io::SeekFrom::Start(0)).await.map_err(RemoteError::Io)?;
    let total = file.metadata().await.map(|m| m.len()).ok();

    let body = Body::from_stream(SpoolStream { inner: ReaderStream::new(file), _spool: spool });
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/zip"));
    headers.insert(header::CONTENT_DISPOSITION, attachment_disposition("ferrite_archive.zip"));
    if let Some(total) = total {
        headers.insert(header::CONTENT_LENGTH, total.to_string().parse().unwrap());
    }
    Ok(response)
}

async fn zip_stream_entry<W: std::io::Write + std::io::Seek>(
    backend: &dyn RemoteFilesystem,
    writer: &mut zip::ZipWriter<W>,
    source_path: &str,
    archive_name: &str,
    options: zip::write::SimpleFileOptions,
) -> Result<(), RemoteError> {
    use std::io::Write;
    let mut stream = backend.stream_file(source_path).await?;
    writer
        .start_file(archive_name, options)
        .map_err(|e| RemoteError::OperationFailed(format!("Zip entry error: {}", e)))?;
    while let Some(chunk) = stream.next().await {
        writer.write_all(&chunk?).map_err(RemoteError::Io)?;
    }
    Ok(())
}

async fn zip_add_path<W: std::io::Write + std::io::Seek>(
    backend: &dyn RemoteFilesystem,
    writer: &mut zip::ZipWriter<W>,
    target_path: &str,
    options: zip::write::SimpleFileOptions,
) -> Result<(), RemoteError> {
    let is_dir = backend.stat(target_path).await.map(|m| m.is_dir).unwrap_or(false);

    if !is_dir {
        let name = Path::new(target_path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".to_string());
        return zip_stream_entry(backend, writer, target_path, &name.replace('\\', "/"), options).await;
    }

    let root_parent = Path::new(target_path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut queue = vec![target_path.to_string()];
    while let Some(dir_path) = queue.pop() {
        let Ok(entries) = backend.list_dir(&dir_path).await else { continue };
        for entry in entries {
            if entry.is_dir {
                queue.push(entry.path);
                continue;
            }
            let rel_path = if !root_parent.is_empty() && entry.path.starts_with(&root_parent) {
                entry.path[root_parent.len()..].trim_start_matches(['/', '\\']).to_string()
            } else {
                entry.name.clone()
            };
            if let Err(e) = zip_stream_entry(backend, writer, &entry.path, &rel_path.replace('\\', "/"), options).await {
                tracing::warn!("Skipping '{}' in zip: {}", entry.path, e);
            }
        }
    }

    Ok(())
}

pub async fn search_files(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<SearchQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, &query.remote, &query.path, false)?;
    let backend = state.get_backend(&query.remote)?;
    let matches = backend.search(&query.path, &query.query, query.content.unwrap_or(false)).await?;
    Ok(Json(matches))
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct GlobalSearchQuery {
    pub remote: String,
    pub path: String,
    pub query: String,
    pub case_sensitive: Option<bool>,
    pub is_regex: Option<bool>,
    pub includes: Option<String>,
    pub excludes: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SearchMatchLine {
    pub file: String,
    pub line: usize,
    pub text: String,
}

pub async fn global_search(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<GlobalSearchQuery>,
) -> Result<Json<Vec<SearchMatchLine>>, ApiError> {
    let user = state.current_user(&cookies)?;
    // This handler shells out to `rg` against the server's local filesystem
    // regardless of `query.remote`, so it is authorized against "local".
    authorize(&user, "local", &query.path, false)?;
    if query.query.is_empty() {
        return Ok(Json(Vec::new()));
    }

    let search_path = local_disk_path(&state, &query.path)?;
    let search_dir = search_path.as_path();
    if !search_dir.exists() {
        return Ok(Json(Vec::new()));
    }
    let target_dir = if search_dir.is_file() { search_dir.parent().unwrap_or(search_dir) } else { search_dir };

    let mut args: Vec<String> = vec![
        "--no-heading".into(),
        "--line-number".into(),
        "--color=never".into(),
        "--max-count=10".into(),
        "--max-filesize=2M".into(),
    ];
    if !query.case_sensitive.unwrap_or(false) {
        args.push("-i".into());
    }
    if !query.is_regex.unwrap_or(false) {
        args.push("-F".into());
    }
    for glob in query.includes.iter().flat_map(|s| s.split(',')).map(str::trim).filter(|s| !s.is_empty()) {
        args.push("-g".into());
        args.push(glob.to_string());
    }
    for glob in query.excludes.iter().flat_map(|s| s.split(',')).map(str::trim).filter(|s| !s.is_empty()) {
        args.push("-g".into());
        args.push(format!("!{}", glob));
    }
    args.push("--".into());
    args.push(query.query.clone());
    args.push(".".into());

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new("rg").args(&args).current_dir(target_dir).output(),
    )
    .await;

    let mut results = Vec::new();
    let max_results = 200;
    let target_base = target_dir.to_string_lossy().replace('\\', "/");

    if let Ok(Ok(out)) = output {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if results.len() >= max_results {
                break;
            }
            let line_clean = line.trim_start_matches("./");
            let Some((file_rel, rest)) = line_clean.split_once(':') else { continue };
            let Some((line_no, match_text)) = rest.split_once(':') else { continue };
            let Ok(ln) = line_no.parse::<usize>() else { continue };
            results.push(SearchMatchLine {
                file: format!("{}/{}", target_base.trim_end_matches('/'), file_rel.replace('\\', "/")),
                line: ln,
                text: match_text.chars().take(200).collect(),
            });
        }
    }

    Ok(Json(results))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_parsing_handles_open_suffix_and_bounds() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=900-", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=0-5000", 1000), Some((0, 999)));
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=5-2", 1000), None);
        assert_eq!(parse_range("bytes=0-", 0), None);
        assert_eq!(parse_range("items=0-1", 10), None);
    }

    #[test]
    fn range_length_is_capped() {
        let total = 10 * MAX_RANGE_BYTES;
        assert_eq!(parse_range("bytes=0-", total), Some((0, MAX_RANGE_BYTES - 1)));
    }

    #[test]
    fn disposition_never_carries_a_quote_or_control_character() {
        let v = attachment_disposition("we\"ird\r\nname é.txt");
        let s = v.to_str().unwrap();
        assert!(s.starts_with("attachment; filename=\"we_ird__name _.txt\""), "{s}");
        assert!(s.contains("filename*=UTF-8''we%22ird%0D%0Aname%20%C3%A9.txt"), "{s}");
    }
}
