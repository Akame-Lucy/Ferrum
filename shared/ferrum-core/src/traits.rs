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
    async fn stat(&self, path: &str) -> Result<FileMeta, RemoteError>;
    async fn delete(&self, path: &str) -> Result<(), RemoteError>;
    async fn rename(&self, from: &str, to: &str) -> Result<(), RemoteError>;
    async fn create_file(&self, path: &str) -> Result<(), RemoteError>;
    async fn create_dir(&self, path: &str) -> Result<(), RemoteError>;
    async fn search(&self, path: &str, query: &str, content_search: bool) -> Result<Vec<SearchMatch>, RemoteError>;
}
