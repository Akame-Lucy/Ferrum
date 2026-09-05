use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use thiserror::Error;

pub type BoxStream<'a, T> = Pin<Box<dyn Stream<Item = T> + Send + 'a>>;

#[derive(Debug, Error)]
pub enum RemoteError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("SSH error: {0}")]
    Ssh(String),

    #[error("Authentication failed for remote '{0}'")]
    AuthFailed(String),

    #[error("Connection failed: {0}")]
    ConnectionFailed(String),

    #[error("File not found: {0}")]
    NotFound(String),

    #[error("Remote not configured: {0}")]
    RemoteNotFound(String),

    #[error("Operation failed: {0}")]
    OperationFailed(String),

    /// The request itself was malformed (bad name, missing field, value out
    /// of range). Distinct from `OperationFailed` so the HTTP layer can say
    /// 400 rather than blaming the server.
    #[error("Invalid request: {0}")]
    InvalidInput(String),

    #[error("Unauthorized access: {0}")]
    Unauthorized(String),

    /// Distinct from `Unauthorized`: the caller is authenticated, but their
    /// account's permission grants don't cover this remote/path.
    #[error("Forbidden: {0}")]
    Forbidden(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMeta {
    pub size: u64,
    pub is_dir: bool,
    pub modified: Option<u64>,
    pub permissions: Option<u32>,
}

impl FileMeta {
    /// Metadata of an entry on the local filesystem of whichever twin is
    /// asking. Permission bits are reported where the platform has them.
    pub fn from_metadata(meta: &std::fs::Metadata) -> Self {
        Self {
            size: meta.len(),
            is_dir: meta.is_dir(),
            modified: modified_secs(meta),
            permissions: unix_mode(meta),
        }
    }
}

/// Seconds since the Unix epoch of a local entry's modification time.
pub fn modified_secs(meta: &std::fs::Metadata) -> Option<u64> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

#[cfg(unix)]
fn unix_mode(meta: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn unix_mode(_meta: &std::fs::Metadata) -> Option<u32> {
    None
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchMatch {
    pub path: String,
    pub line_number: Option<usize>,
    pub snippet: Option<String>,
}

#[async_trait]
pub trait RemoteFilesystem: Send + Sync {
    async fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>, RemoteError>;
    async fn read_file(&self, path: &str) -> Result<Vec<u8>, RemoteError>;
    async fn stream_file(&self, path: &str) -> Result<BoxStream<'static, Result<Bytes, RemoteError>>, RemoteError> {
        let content = self.read_file(path).await?;
        let bytes = Bytes::from(content);
        Ok(Box::pin(futures_util::stream::once(async move { Ok(bytes) })))
    }
    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), RemoteError>;

    /// Writes `path` from a stream of chunks. Backends that can write
    /// incrementally (local disk, a Ferrous agent) override this so an upload
    /// never has to sit in memory whole; the default collects the stream and
    /// calls `write_file`, which is correct for every backend and merely
    /// costs memory proportional to the file.
    async fn write_stream(
        &self,
        path: &str,
        mut stream: BoxStream<'static, Result<Bytes, RemoteError>>,
    ) -> Result<(), RemoteError> {
        use futures_util::StreamExt;
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk?);
        }
        self.write_file(path, &buf).await
    }
    async fn stat(&self, path: &str) -> Result<FileMeta, RemoteError>;
    async fn delete(&self, path: &str) -> Result<(), RemoteError>;
    async fn rename(&self, from: &str, to: &str) -> Result<(), RemoteError>;
    async fn create_file(&self, path: &str) -> Result<(), RemoteError>;
    async fn create_dir(&self, path: &str) -> Result<(), RemoteError>;
    async fn search(&self, path: &str, query: &str, content_search: bool) -> Result<Vec<SearchMatch>, RemoteError>;

    /// The real on-disk path for `path`, for backends that are a directory
    /// on the machine Ferrite runs on; `None` for every remote backend.
    /// Handlers that must open the disk directly (HTTP range reads, git,
    /// zip inspection) resolve through this rather than trusting the raw
    /// request path.
    fn local_path(&self, _path: &str) -> Option<std::path::PathBuf> {
        None
    }
}
