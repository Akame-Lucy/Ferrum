use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use async_trait::async_trait;
use ssh2::Session;
use tokio::task;

use crate::config::RemoteConfig;
use crate::traits::{FileEntry, FileMeta, RemoteError, RemoteFilesystem, SearchMatch};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const SESSION_TIMEOUT_MS: u32 = 30_000;

type SessionCache = Arc<Mutex<Option<(Session, ssh2::Sftp)>>>;

struct PasswordPrompt(String);

impl ssh2::KeyboardInteractivePrompt for PasswordPrompt {
    fn prompt(
        &mut self,
        _username: &str,
        _instructions: &str,
        _prompts: &[ssh2::Prompt],
    ) -> Vec<String> {
        vec![self.0.clone()]
    }
}

pub struct SftpBackend {
    config: RemoteConfig,
    session_cache: SessionCache,
}

impl SftpBackend {
    pub fn new(config: RemoteConfig) -> Self {
        Self {
            config,
            session_cache: Arc::new(Mutex::new(None)),
        }
    }
}

fn resolve_addr(host: &str, port: u16) -> Result<std::net::SocketAddr, RemoteError> {
    let addr = format!("{}:{}", host, port);
    addr.to_socket_addrs()
        .map_err(|e| RemoteError::ConnectionFailed(format!("Failed to resolve {}: {}", addr, e)))?
        .next()
        .ok_or_else(|| RemoteError::ConnectionFailed(format!("No address found for {}", addr)))
}

fn create_connection(config: &RemoteConfig) -> Result<(Session, ssh2::Sftp), RemoteError> {
    let port = if config.port == 0 { 22 } else { config.port };
    let sock_addr = resolve_addr(&config.host, port)?;

    let tcp = TcpStream::connect_timeout(&sock_addr, CONNECT_TIMEOUT)
        .map_err(|e| RemoteError::ConnectionFailed(format!("Failed to connect to {}: {}", sock_addr, e)))?;

    let mut sess = Session::new()
        .map_err(|e| RemoteError::Ssh(format!("Failed to create SSH session: {}", e)))?;
    sess.set_timeout(SESSION_TIMEOUT_MS);
    sess.set_tcp_stream(tcp);
    sess.handshake()
        .map_err(|e| RemoteError::Ssh(format!("SSH handshake failed: {}", e)))?;

    if let Some(key_path) = config.auth.key_path() {
        let expanded_path = shellexpand_path(key_path);
        let path = Path::new(&expanded_path);
        if path.exists() {
            let _ = sess.userauth_pubkey_file(
                &config.username,
                None,
                path,
                config.auth.passphrase.as_deref(),
            );
        }
    }

    if !sess.authenticated() {
        if let Some(ref password) = config.auth.password {
            let _ = sess.userauth_password(&config.username, password);
        }
    }

    if !sess.authenticated() {
        if let Some(ref password) = config.auth.password {
            let mut prompt = PasswordPrompt(password.clone());
            let _ = sess.userauth_keyboard_interactive(&config.username, &mut prompt);
        }
    }

    if !sess.authenticated() {
        let _ = sess.userauth_agent(&config.username);
    }

    if !sess.authenticated() {
        return Err(RemoteError::AuthFailed(format!("Authentication failed for {}", config.username)));
    }

    let sftp = sess.sftp()
        .map_err(|e| RemoteError::Ssh(format!("Failed to start SFTP subsystem: {}", e)))?;

    Ok((sess, sftp))
}

fn with_sftp<F, R>(config: &RemoteConfig, cache: &SessionCache, f: F) -> Result<R, RemoteError>
where
    F: FnOnce(&ssh2::Sftp) -> Result<R, RemoteError>,
{
    let mut guard = cache.lock().map_err(|_| RemoteError::OperationFailed("Lock poisoned".into()))?;

    if let Some((sess, sftp)) = guard.as_ref() {
        if sess.authenticated() && sftp.stat(Path::new(".")).is_ok() {
            return f(sftp);
        }
    }

    *guard = None;
    let (sess, sftp) = create_connection(config)?;
    let res = f(&sftp);
    *guard = Some((sess, sftp));
    res
}

fn to_posix_path(p: &str) -> String {
    let clean = p.replace('\\', "/");
    if clean.is_empty() {
        "/".to_string()
    } else if !clean.starts_with('/') {
        format!("/{}", clean)
    } else {
        clean
    }
}

fn shellexpand_path(p: &str) -> String {
    if p.starts_with("~/") || p == "~" {
        if let Some(home) = dirs_home() {
            return p.replacen('~', &home, 1);
        }
    }
    p.to_string()
}

fn dirs_home() -> Option<String> {
    std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).ok()
}

fn search_sftp_dir(
    sftp: &ssh2::Sftp,
    dir_path_str: &str,
    query_lower: &str,
    content_search: bool,
    results: &mut Vec<SearchMatch>,
) {
    let posix_dir = to_posix_path(dir_path_str);
    let entries = match sftp.readdir(Path::new(&posix_dir)) {
        Ok(e) => e,
        Err(_) => return,
    };

    for (path_buf, stat) in entries {
        let file_name = path_buf.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if file_name == "." || file_name == ".." || file_name.is_empty() {
            continue;
        }

        let full_path = if posix_dir.ends_with('/') {
            format!("{}{}", posix_dir, file_name)
        } else {
            format!("{}/{}", posix_dir, file_name)
        };

        if stat.is_dir() {
            search_sftp_dir(sftp, &full_path, query_lower, content_search, results);
        } else {
            if content_search {
                if let Ok(mut file) = sftp.open(Path::new(&full_path)) {
                    let mut contents = Vec::new();
                    if file.read_to_end(&mut contents).is_ok() {
                        if let Ok(text) = std::str::from_utf8(&contents) {
                            for (idx, line) in text.lines().enumerate() {
                                if line.to_lowercase().contains(query_lower) {
                                    results.push(SearchMatch {
                                        path: full_path.clone(),
                                        line_number: Some(idx + 1),
                                        snippet: Some(line.trim().to_string()),
                                    });
                                }
                            }
                        }
                    }
                }
            } else {
                if file_name.to_lowercase().contains(query_lower) || full_path.to_lowercase().contains(query_lower) {
                    results.push(SearchMatch {
                        path: full_path,
                        line_number: None,
                        snippet: None,
                    });
                }
            }
        }
    }
}

#[async_trait]
impl RemoteFilesystem for SftpBackend {
    async fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>, RemoteError> {
        let config = self.config.clone();
        let cache = Arc::clone(&self.session_cache);
        let target_posix = to_posix_path(path);

        task::spawn_blocking(move || {
            with_sftp(&config, &cache, |sftp| {
                let entries = sftp.readdir(Path::new(&target_posix))
                    .map_err(|e| RemoteError::OperationFailed(format!("Failed to list dir '{}': {}", target_posix, e)))?;

                let mut result = Vec::new();
                for (path_buf, stat) in entries {
                    let file_name = path_buf.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if file_name == "." || file_name == ".." {
                        continue;
                    }
                    let full_path = if target_posix.ends_with('/') {
                        format!("{}{}", target_posix, file_name)
                    } else {
                        format!("{}/{}", target_posix, file_name)
                    };

                    let is_dir = stat.is_dir();
                    let size = stat.size.unwrap_or(0);
                    let modified = stat.mtime;

                    result.push(FileEntry {
                        name: file_name,
                        path: full_path,
                        is_dir,
                        size,
                        modified,
                    });
                }

                result.sort_by(|a, b| {
                    if a.is_dir != b.is_dir {
                        b.is_dir.cmp(&a.is_dir)
                    } else {
                        a.name.to_lowercase().cmp(&b.name.to_lowercase())
                    }
                });

                Ok(result)
            })
        }).await.map_err(|e| RemoteError::OperationFailed(e.to_string()))?
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, RemoteError> {
        let config = self.config.clone();
        let cache = Arc::clone(&self.session_cache);
        let target_posix = to_posix_path(path);

        task::spawn_blocking(move || {
            with_sftp(&config, &cache, |sftp| {
                let mut file = sftp.open(Path::new(&target_posix))
                    .map_err(|e| RemoteError::NotFound(format!("Failed to open file '{}': {}", target_posix, e)))?;

                let mut contents = Vec::new();
                file.read_to_end(&mut contents)
                    .map_err(RemoteError::Io)?;

                Ok(contents)
            })
        }).await.map_err(|e| RemoteError::OperationFailed(e.to_string()))?
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), RemoteError> {
        let config = self.config.clone();
        let cache = Arc::clone(&self.session_cache);
        let target_posix = to_posix_path(path);
        let data = contents.to_vec();

        task::spawn_blocking(move || {
            with_sftp(&config, &cache, |sftp| {
                let mut file = sftp.create(Path::new(&target_posix))
                    .map_err(|e| RemoteError::OperationFailed(format!("Failed to create file '{}': {}", target_posix, e)))?;

                file.write_all(&data)
                    .map_err(RemoteError::Io)?;

                Ok(())
            })
        }).await.map_err(|e| RemoteError::OperationFailed(e.to_string()))?
    }

    async fn stat(&self, path: &str) -> Result<FileMeta, RemoteError> {
        let config = self.config.clone();
        let cache = Arc::clone(&self.session_cache);
        let target_posix = to_posix_path(path);

        task::spawn_blocking(move || {
            with_sftp(&config, &cache, |sftp| {
                let stat = sftp.stat(Path::new(&target_posix))
                    .map_err(|e| RemoteError::NotFound(format!("Failed to stat path '{}': {}", target_posix, e)))?;

                Ok(FileMeta {
                    size: stat.size.unwrap_or(0),
                    is_dir: stat.is_dir(),
                    modified: stat.mtime,
                    permissions: stat.perm,
                })
            })
        }).await.map_err(|e| RemoteError::OperationFailed(e.to_string()))?
    }

    async fn delete(&self, path: &str) -> Result<(), RemoteError> {
        let config = self.config.clone();
        let cache = Arc::clone(&self.session_cache);
        let target_posix = to_posix_path(path);

        task::spawn_blocking(move || {
            with_sftp(&config, &cache, |sftp| {
                let stat = sftp.stat(Path::new(&target_posix))
                    .map_err(|e| RemoteError::NotFound(format!("Failed to stat path '{}': {}", target_posix, e)))?;

                if stat.is_dir() {
                    sftp.rmdir(Path::new(&target_posix))
                        .map_err(|e| RemoteError::OperationFailed(format!("Failed to delete directory '{}': {}", target_posix, e)))?;
                } else {
                    sftp.unlink(Path::new(&target_posix))
                        .map_err(|e| RemoteError::OperationFailed(format!("Failed to delete file '{}': {}", target_posix, e)))?;
                }

                Ok(())
            })
        }).await.map_err(|e| RemoteError::OperationFailed(e.to_string()))?
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), RemoteError> {
        let config = self.config.clone();
        let cache = Arc::clone(&self.session_cache);
        let src_posix = to_posix_path(from);
        let dst_posix = to_posix_path(to);

        task::spawn_blocking(move || {
            with_sftp(&config, &cache, |sftp| {
                sftp.rename(Path::new(&src_posix), Path::new(&dst_posix), None)
                    .map_err(|e| RemoteError::OperationFailed(format!("Failed to rename from '{}' to '{}': {}", src_posix, dst_posix, e)))?;

                Ok(())
            })
        }).await.map_err(|e| RemoteError::OperationFailed(e.to_string()))?
    }

    async fn create_file(&self, path: &str) -> Result<(), RemoteError> {
        self.write_file(path, &[]).await
    }

    async fn create_dir(&self, path: &str) -> Result<(), RemoteError> {
        let config = self.config.clone();
        let cache = Arc::clone(&self.session_cache);
        let target_posix = to_posix_path(path);

        task::spawn_blocking(move || {
            with_sftp(&config, &cache, |sftp| {
                sftp.mkdir(Path::new(&target_posix), 0o755)
                    .map_err(|e| RemoteError::OperationFailed(format!("Failed to create directory '{}': {}", target_posix, e)))?;

                Ok(())
            })
        }).await.map_err(|e| RemoteError::OperationFailed(e.to_string()))?
    }

    async fn search(&self, path: &str, query: &str, content_search: bool) -> Result<Vec<SearchMatch>, RemoteError> {
        let config = self.config.clone();
        let cache = Arc::clone(&self.session_cache);
        let target_posix = to_posix_path(path);
        let query_str = query.to_lowercase();

        task::spawn_blocking(move || {
            with_sftp(&config, &cache, |sftp| {
                let mut results = Vec::new();
                search_sftp_dir(sftp, &target_posix, &query_str, content_search, &mut results);
                Ok(results)
            })
        }).await.map_err(|e| RemoteError::OperationFailed(e.to_string()))?
    }
}
