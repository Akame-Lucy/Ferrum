use std::io::Cursor;
use std::net::ToSocketAddrs;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use async_trait::async_trait;
use suppaftp::types::FileType;
use suppaftp::{FtpStream, Mode};
use tokio::task;

use crate::config::RemoteConfig;
use crate::traits::{FileEntry, FileMeta, RemoteError, RemoteFilesystem, SearchMatch};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(30);

type FtpCache = Arc<Mutex<Option<FtpStream>>>;

pub struct FtpBackend {
    config: RemoteConfig,
    session_cache: FtpCache,
}

impl FtpBackend {
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

fn create_connection(config: &RemoteConfig) -> Result<FtpStream, RemoteError> {
    let port = if config.port == 22 || config.port == 0 { 21 } else { config.port };
    let sock_addr = resolve_addr(&config.host, port)?;

    let mut ftp = FtpStream::connect_timeout(sock_addr, CONNECT_TIMEOUT)
        .map_err(|e| RemoteError::ConnectionFailed(format!("Failed to connect to FTP {}: {}", sock_addr, e)))?;

    let stream = ftp.get_ref();
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

    ftp.set_mode(Mode::Passive);

    let password = config.auth.password.as_deref().unwrap_or_default();
    ftp.login(config.username.as_str(), password)
        .map_err(|e| RemoteError::AuthFailed(format!("FTP auth failed for {}: {}", config.username, e)))?;

    ftp.transfer_type(FileType::Binary)
        .map_err(|e| RemoteError::OperationFailed(format!("Failed to set binary mode: {}", e)))?;

    Ok(ftp)
}

fn with_ftp<F, R>(config: &RemoteConfig, cache: &FtpCache, mut f: F) -> Result<R, RemoteError>
where
    F: FnMut(&mut FtpStream) -> Result<R, RemoteError>,
{
    let mut guard = cache.lock().map_err(|_| RemoteError::OperationFailed("Lock poisoned".into()))?;

    if let Some(ftp) = guard.as_mut() {
        if ftp.pwd().is_ok() {
            return f(ftp);
        }
    }

    *guard = None;
    let mut fresh_ftp = create_connection(config)?;
    let res = f(&mut fresh_ftp);
    *guard = Some(fresh_ftp);
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

fn is_directory(ftp: &mut FtpStream, full_path: &str) -> bool {
    if let Ok(current_pwd) = ftp.pwd() {
        if ftp.cwd(full_path).is_ok() {
            let _ = ftp.cwd(&current_pwd);
            return true;
        }
    }
    false
}

fn search_ftp_dir(
    ftp: &mut FtpStream,
    path: &str,
    query_lower: &str,
    content_search: bool,
    results: &mut Vec<SearchMatch>,
) {
    let posix_dir = to_posix_path(path);
    let list = match ftp.nlst(Some(&posix_dir)) {
        Ok(l) => l,
        Err(_) => return,
    };

    for item in list {
        let name = item.split('/').next_back().unwrap_or(&item).to_string();
        if name == "." || name == ".." || name.is_empty() {
            continue;
        }

        let full_path = if posix_dir.ends_with('/') {
            format!("{}{}", posix_dir, name)
        } else {
            format!("{}/{}", posix_dir, name)
        };

        let is_dir = is_directory(ftp, &full_path);

        if is_dir {
            search_ftp_dir(ftp, &full_path, query_lower, content_search, results);
        } else {
            if content_search {
                if let Ok(cursor) = ftp.retr_as_buffer(&full_path) {
                    let bytes = cursor.into_inner();
                    if let Ok(text) = std::str::from_utf8(&bytes) {
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
            } else {
                if name.to_lowercase().contains(query_lower) || full_path.to_lowercase().contains(query_lower) {
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
impl RemoteFilesystem for FtpBackend {
    async fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>, RemoteError> {
        let config = self.config.clone();
        let cache = Arc::clone(&self.session_cache);
        let target_posix = to_posix_path(path);

        task::spawn_blocking(move || {
            with_ftp(&config, &cache, |ftp| {
                let list = ftp.nlst(Some(&target_posix))
                    .map_err(|e| RemoteError::OperationFailed(format!("Failed to list FTP dir '{}': {}", target_posix, e)))?;

                let mut result = Vec::new();
                for item in list {
                    let name = item.split('/').next_back().unwrap_or(&item).to_string();
                    if name == "." || name == ".." || name.is_empty() {
                        continue;
                    }
                    let full_path = if target_posix.ends_with('/') {
                        format!("{}{}", target_posix, name)
                    } else {
                        format!("{}/{}", target_posix, name)
                    };

                    let is_dir = is_directory(ftp, &full_path);
                    let size = if !is_dir { ftp.size(&full_path).ok().unwrap_or(0) as u64 } else { 0 };

                    result.push(FileEntry {
                        name,
                        path: full_path,
                        is_dir,
                        size,
                        modified: None,
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
            with_ftp(&config, &cache, |ftp| {
                let cursor = ftp.retr_as_buffer(&target_posix)
                    .map_err(|e| RemoteError::NotFound(format!("Failed to read FTP file '{}': {}", target_posix, e)))?;
                Ok(cursor.into_inner())
            })
        }).await.map_err(|e| RemoteError::OperationFailed(e.to_string()))?
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), RemoteError> {
        let config = self.config.clone();
        let cache = Arc::clone(&self.session_cache);
        let target_posix = to_posix_path(path);
        let data = contents.to_vec();

        task::spawn_blocking(move || {
            with_ftp(&config, &cache, |ftp| {
                let mut cursor = Cursor::new(&data);
                ftp.put_file(&target_posix, &mut cursor)
                    .map_err(|e| RemoteError::OperationFailed(format!("Failed to write FTP file '{}': {}", target_posix, e)))?;
                Ok(())
            })
        }).await.map_err(|e| RemoteError::OperationFailed(e.to_string()))?
    }

    async fn stat(&self, path: &str) -> Result<FileMeta, RemoteError> {
        let config = self.config.clone();
        let cache = Arc::clone(&self.session_cache);
        let target_posix = to_posix_path(path);

        task::spawn_blocking(move || {
            with_ftp(&config, &cache, |ftp| {
                let is_dir = is_directory(ftp, &target_posix);
                let size = if !is_dir { ftp.size(&target_posix).ok().unwrap_or(0) as u64 } else { 0 };

                Ok(FileMeta {
                    size,
                    is_dir,
                    modified: None,
                    permissions: None,
                })
            })
        }).await.map_err(|e| RemoteError::OperationFailed(e.to_string()))?
    }

    async fn delete(&self, path: &str) -> Result<(), RemoteError> {
        let config = self.config.clone();
        let cache = Arc::clone(&self.session_cache);
        let target_posix = to_posix_path(path);

        task::spawn_blocking(move || {
            with_ftp(&config, &cache, |ftp| {
                if ftp.rm(&target_posix).is_err() {
                    ftp.rmdir(&target_posix)
                        .map_err(|e| RemoteError::OperationFailed(format!("Failed to delete FTP path '{}': {}", target_posix, e)))?;
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
            with_ftp(&config, &cache, |ftp| {
                ftp.rename(&src_posix, &dst_posix)
                    .map_err(|e| RemoteError::OperationFailed(format!("Failed to rename FTP path from '{}' to '{}': {}", src_posix, dst_posix, e)))?;
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
            with_ftp(&config, &cache, |ftp| {
                ftp.mkdir(&target_posix)
                    .map_err(|e| RemoteError::OperationFailed(format!("Failed to create FTP directory '{}': {}", target_posix, e)))?;
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
            with_ftp(&config, &cache, |ftp| {
                let mut results = Vec::new();
                search_ftp_dir(ftp, &target_posix, &query_str, content_search, &mut results);
                Ok(results)
            })
        }).await.map_err(|e| RemoteError::OperationFailed(e.to_string()))?
    }
}
