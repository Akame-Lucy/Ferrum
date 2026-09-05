use std::io::Cursor;
use std::net::ToSocketAddrs;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use suppaftp::types::FileType;
use suppaftp::{FtpStream, ImplFtpStream, Mode, NativeTlsConnector, NativeTlsFtpStream, TlsStream};
use tokio::task;

use crate::config::{ProtocolType, RemoteConfig};
use crate::traits::{FileEntry, FileMeta, RemoteError, RemoteFilesystem, SearchMatch};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// A control connection, plain for `ftp` and TLS-wrapped for `ftps`. Both
/// variants expose the same command set through `ImplFtpStream`; the
/// `dispatch!` macro below runs one generic operation on whichever is live.
enum Ftp {
    Plain(FtpStream),
    Tls(NativeTlsFtpStream),
}

macro_rules! dispatch {
    ($ftp:expr, $s:ident => $body:expr) => {
        match $ftp {
            Ftp::Plain($s) => $body,
            Ftp::Tls($s) => $body,
        }
    };
}

type FtpCache = Arc<Mutex<Option<Ftp>>>;

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

fn create_connection(config: &RemoteConfig) -> Result<Ftp, RemoteError> {
    let port = if config.port == 22 || config.port == 0 { 21 } else { config.port };
    let sock_addr = resolve_addr(&config.host, port)?;

    let connect_failed = |e: suppaftp::FtpError| {
        RemoteError::ConnectionFailed(format!("Failed to connect to FTP {}: {}", sock_addr, e))
    };
    let set_timeouts = |stream: &std::net::TcpStream| {
        let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    };

    // `ftps` is explicit FTPS: AUTH TLS on the control connection, then
    // PROT P so data connections are encrypted too. The certificate is
    // verified against the system trust store for the configured host.
    let mut ftp = if config.protocol == ProtocolType::Ftps {
        let ftp = NativeTlsFtpStream::connect_timeout(sock_addr, CONNECT_TIMEOUT).map_err(connect_failed)?;
        set_timeouts(ftp.get_ref());
        let connector = native_tls::TlsConnector::new()
            .map_err(|e| RemoteError::ConnectionFailed(format!("TLS setup failed: {}", e)))?;
        let secured = ftp
            .into_secure(NativeTlsConnector::from(connector), &config.host)
            .map_err(|e| RemoteError::ConnectionFailed(format!("FTPS negotiation with {} failed: {}", config.host, e)))?;
        Ftp::Tls(secured)
    } else {
        let ftp = FtpStream::connect_timeout(sock_addr, CONNECT_TIMEOUT).map_err(connect_failed)?;
        set_timeouts(ftp.get_ref());
        Ftp::Plain(ftp)
    };

    dispatch!(&mut ftp, s => login(s, config))?;
    Ok(ftp)
}

fn login<T: TlsStream>(ftp: &mut ImplFtpStream<T>, config: &RemoteConfig) -> Result<(), RemoteError> {
    ftp.set_mode(Mode::Passive);
    let password = config.auth.password.as_deref().unwrap_or_default();
    ftp.login(config.username.as_str(), password)
        .map_err(|e| RemoteError::AuthFailed(format!("FTP auth failed for {}: {}", config.username, e)))?;
    ftp.transfer_type(FileType::Binary)
        .map_err(|e| RemoteError::OperationFailed(format!("Failed to set binary mode: {}", e)))
}

fn with_ftp<F, R>(config: &RemoteConfig, cache: &FtpCache, mut f: F) -> Result<R, RemoteError>
where
    F: FnMut(&mut Ftp) -> Result<R, RemoteError>,
{
    let mut guard = cache.lock().map_err(|_| RemoteError::OperationFailed("Lock poisoned".into()))?;

    if let Some(ftp) = guard.as_mut() {
        if dispatch!(&mut *ftp, s => s.pwd()).is_ok() {
            return f(ftp);
        }
    }

    *guard = None;
    let mut fresh = create_connection(config)?;
    let res = f(&mut fresh);
    *guard = Some(fresh);
    res
}

/// A browsed path as the server wants it. FTP commands are lines, so a path
/// carrying a CR or LF would end the command early and start another; such
/// a path is refused rather than sent.
fn to_posix_path(p: &str) -> Result<String, RemoteError> {
    if p.contains(['\r', '\n', '\0']) {
        return Err(RemoteError::InvalidInput("Path contains a control character".into()));
    }
    let clean = p.replace('\\', "/");
    Ok(if clean.is_empty() {
        "/".to_string()
    } else if !clean.starts_with('/') {
        format!("/{}", clean)
    } else {
        clean
    })
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{}{}", dir, name)
    } else {
        format!("{}/{}", dir, name)
    }
}

fn is_directory<T: TlsStream>(ftp: &mut ImplFtpStream<T>, full_path: &str) -> bool {
    if let Ok(current_pwd) = ftp.pwd() {
        if ftp.cwd(full_path).is_ok() {
            let _ = ftp.cwd(&current_pwd);
            return true;
        }
    }
    false
}

/// Names in `dir`, without the `.`/`..` entries some servers include.
fn entry_names<T: TlsStream>(ftp: &mut ImplFtpStream<T>, dir: &str) -> Result<Vec<String>, RemoteError> {
    let list = ftp
        .nlst(Some(dir))
        .map_err(|e| RemoteError::OperationFailed(format!("Failed to list FTP dir '{}': {}", dir, e)))?;
    Ok(list
        .into_iter()
        .map(|item| item.rsplit('/').next().unwrap_or(&item).to_string())
        .filter(|name| !name.is_empty() && name != "." && name != "..")
        .collect())
}

fn list_dir<T: TlsStream>(ftp: &mut ImplFtpStream<T>, dir: &str) -> Result<Vec<FileEntry>, RemoteError> {
    let mut result = Vec::new();
    for name in entry_names(ftp, dir)? {
        let full_path = join(dir, &name);
        let is_dir = is_directory(ftp, &full_path);
        let size = if is_dir { 0 } else { ftp.size(&full_path).unwrap_or(0) as u64 };
        result.push(FileEntry { name, path: full_path, is_dir, size, modified: None });
    }
    result.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(result)
}

fn search_dir<T: TlsStream>(
    ftp: &mut ImplFtpStream<T>,
    dir: &str,
    query_lower: &str,
    content_search: bool,
    results: &mut Vec<SearchMatch>,
) {
    let Ok(names) = entry_names(ftp, dir) else { return };
    for name in names {
        let full_path = join(dir, &name);
        if is_directory(ftp, &full_path) {
            search_dir(ftp, &full_path, query_lower, content_search, results);
        } else if content_search {
            let Ok(cursor) = ftp.retr_as_buffer(&full_path) else { continue };
            let bytes = cursor.into_inner();
            let Ok(text) = std::str::from_utf8(&bytes) else { continue };
            for (idx, line) in text.lines().enumerate() {
                if line.to_lowercase().contains(query_lower) {
                    results.push(SearchMatch {
                        path: full_path.clone(),
                        line_number: Some(idx + 1),
                        snippet: Some(line.trim().to_string()),
                    });
                }
            }
        } else if name.to_lowercase().contains(query_lower) || full_path.to_lowercase().contains(query_lower) {
            results.push(SearchMatch { path: full_path, line_number: None, snippet: None });
        }
    }
}

impl FtpBackend {
    /// Runs `op` on the cached control connection from a blocking thread,
    /// reconnecting first if the connection has gone stale.
    async fn run<R, F>(&self, op: F) -> Result<R, RemoteError>
    where
        R: Send + 'static,
        F: FnMut(&mut Ftp) -> Result<R, RemoteError> + Send + 'static,
    {
        let config = self.config.clone();
        let cache = Arc::clone(&self.session_cache);
        task::spawn_blocking(move || with_ftp(&config, &cache, op))
            .await
            .map_err(|e| RemoteError::OperationFailed(e.to_string()))?
    }
}

#[async_trait]
impl RemoteFilesystem for FtpBackend {
    async fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>, RemoteError> {
        let dir = to_posix_path(path)?;
        self.run(move |ftp| dispatch!(ftp, s => list_dir(s, &dir))).await
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, RemoteError> {
        let target = to_posix_path(path)?;
        self.run(move |ftp| {
            dispatch!(ftp, s => s.retr_as_buffer(&target))
                .map(|cursor| cursor.into_inner())
                .map_err(|e| RemoteError::NotFound(format!("Failed to read FTP file '{}': {}", target, e)))
        })
        .await
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), RemoteError> {
        let target = to_posix_path(path)?;
        let data = contents.to_vec();
        self.run(move |ftp| {
            let mut cursor = Cursor::new(&data);
            dispatch!(ftp, s => s.put_file(&target, &mut cursor))
                .map(|_| ())
                .map_err(|e| RemoteError::OperationFailed(format!("Failed to write FTP file '{}': {}", target, e)))
        })
        .await
    }

    async fn stat(&self, path: &str) -> Result<FileMeta, RemoteError> {
        let target = to_posix_path(path)?;
        self.run(move |ftp| {
            let is_dir = dispatch!(ftp, s => is_directory(s, &target));
            let size = if is_dir { 0 } else { dispatch!(ftp, s => s.size(&target)).unwrap_or(0) as u64 };
            Ok(FileMeta { size, is_dir, modified: None, permissions: None })
        })
        .await
    }

    async fn delete(&self, path: &str) -> Result<(), RemoteError> {
        let target = to_posix_path(path)?;
        self.run(move |ftp| {
            if dispatch!(ftp, s => s.rm(&target)).is_ok() {
                return Ok(());
            }
            dispatch!(ftp, s => s.rmdir(&target))
                .map_err(|e| RemoteError::OperationFailed(format!("Failed to delete FTP path '{}': {}", target, e)))
        })
        .await
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), RemoteError> {
        let src = to_posix_path(from)?;
        let dst = to_posix_path(to)?;
        self.run(move |ftp| {
            dispatch!(ftp, s => s.rename(&src, &dst)).map_err(|e| {
                RemoteError::OperationFailed(format!("Failed to rename FTP path from '{}' to '{}': {}", src, dst, e))
            })
        })
        .await
    }

    async fn create_file(&self, path: &str) -> Result<(), RemoteError> {
        self.write_file(path, &[]).await
    }

    async fn create_dir(&self, path: &str) -> Result<(), RemoteError> {
        let target = to_posix_path(path)?;
        self.run(move |ftp| {
            dispatch!(ftp, s => s.mkdir(&target))
                .map_err(|e| RemoteError::OperationFailed(format!("Failed to create FTP directory '{}': {}", target, e)))
        })
        .await
    }

    async fn search(&self, path: &str, query: &str, content_search: bool) -> Result<Vec<SearchMatch>, RemoteError> {
        let dir = to_posix_path(path)?;
        let query_lower = query.to_lowercase();
        self.run(move |ftp| {
            let mut results = Vec::new();
            dispatch!(ftp, s => search_dir(s, &dir, &query_lower, content_search, &mut results));
            Ok(results)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_rooted_and_forward_slashed() {
        assert_eq!(to_posix_path("").unwrap(), "/");
        assert_eq!(to_posix_path("docs\\a.txt").unwrap(), "/docs/a.txt");
        assert_eq!(to_posix_path("/already").unwrap(), "/already");
    }

    #[test]
    fn control_characters_never_reach_the_wire() {
        assert!(matches!(to_posix_path("/a\r\nDELE /etc/x"), Err(RemoteError::InvalidInput(_))));
        assert!(to_posix_path("/a\nb").is_err());
        assert!(to_posix_path("/a\0b").is_err());
    }
}
