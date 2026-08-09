use std::path::PathBuf;
use async_trait::async_trait;
use futures_util::StreamExt;
use tokio::fs;
use tokio_util::io::ReaderStream;

use crate::config::RemoteConfig;
use crate::traits::{BoxStream, FileEntry, FileMeta, RemoteError, RemoteFilesystem, SearchMatch};


pub struct LocalBackend {
    _config: RemoteConfig,
    base_dir: PathBuf,
}

impl LocalBackend {
    pub fn new(config: RemoteConfig) -> Self {
        let base_dir = match config.base_path.clone() {
            Some(ref p) if !p.trim().is_empty() => PathBuf::from(p),
            _ => Self::drive_root_base(),
        };
        Self {
            _config: config,
            base_dir,
        }
    }

    fn drive_root_base() -> PathBuf {
        if cfg!(windows) {
            PathBuf::new()
        } else {
            PathBuf::from("/")
        }
    }

    fn is_drive_root(&self) -> bool {
        self.base_dir.as_os_str().is_empty()
    }

    fn resolve_path(&self, rel_path: &str) -> PathBuf {
        let clean = rel_path.trim_start_matches('/').trim_start_matches('\\');

        if self.is_drive_root() {
            if clean.is_empty() {
                return PathBuf::new();
            }
            let bytes = clean.as_bytes();
            if bytes.len() == 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
                return PathBuf::from(format!("{}\\", clean));
            }
            return PathBuf::from(clean);
        }

        if clean.is_empty() {
            self.base_dir.clone()
        } else {
            self.base_dir.join(clean)
        }
    }

    fn list_drives(&self) -> Vec<FileEntry> {
        let mut entries = Vec::new();
        for letter in b'A'..=b'Z' {
            let root = format!("{}:\\", letter as char);
            if std::path::Path::new(&root).exists() {
                let name = format!("{}:", letter as char);
                entries.push(FileEntry {
                    name: name.clone(),
                    path: format!("/{}", name),
                    is_dir: true,
                    size: 0,
                    modified: None,
                });
            }
        }
        entries
    }
}

#[async_trait]
impl RemoteFilesystem for LocalBackend {
    async fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>, RemoteError> {
        if self.is_drive_root() {
            let clean = path.trim_start_matches('/').trim_start_matches('\\');
            if clean.is_empty() {
                return Ok(self.list_drives());
            }
        }

        let target = self.resolve_path(path);
        let mut read_dir = fs::read_dir(&target).await
            .map_err(|e| RemoteError::NotFound(format!("Directory not found '{}': {}", path, e)))?;

        let mut entries = Vec::new();
        while let Ok(Some(entry)) = read_dir.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            let metadata = entry.metadata().await.ok();
            let is_dir = metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false);
            let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);

            let clean_path = path.trim_end_matches('/').trim_end_matches('\\');
            let full_rel_path = if clean_path.is_empty() || clean_path == "/" {
                format!("/{}", name)
            } else {
                format!("{}/{}", clean_path, name)
            };

            entries.push(FileEntry {
                name,
                path: full_rel_path,
                is_dir,
                size,
                modified: None,
            });
        }

        entries.sort_by(|a, b| {
            if a.is_dir != b.is_dir {
                b.is_dir.cmp(&a.is_dir)
            } else {
                a.name.to_lowercase().cmp(&b.name.to_lowercase())
            }
        });

        Ok(entries)
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, RemoteError> {
        let target = self.resolve_path(path);
        fs::read(&target).await
            .map_err(|e| RemoteError::NotFound(format!("File not found '{}': {}", path, e)))
    }

    async fn stream_file(&self, path: &str) -> Result<BoxStream<'static, Result<bytes::Bytes, RemoteError>>, RemoteError> {
        let target = self.resolve_path(path);
        let file = fs::File::open(&target).await
            .map_err(|e| RemoteError::NotFound(format!("File not found '{}': {}", path, e)))?;
        let stream = ReaderStream::new(file)
            .map(|res| res.map_err(RemoteError::Io));
        Ok(Box::pin(stream))
    }


    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), RemoteError> {
        let target = self.resolve_path(path);
        if let Some(parent) = target.parent() {
            let _ = fs::create_dir_all(parent).await;
        }
        fs::write(&target, contents).await
            .map_err(|e| RemoteError::OperationFailed(format!("Failed to write file '{}': {}", path, e)))
    }

    async fn stat(&self, path: &str) -> Result<FileMeta, RemoteError> {
        let target = self.resolve_path(path);
        let meta = fs::metadata(&target).await
            .map_err(|e| RemoteError::NotFound(format!("Path not found '{}': {}", path, e)))?;

        Ok(FileMeta {
            size: meta.len(),
            is_dir: meta.is_dir(),
            modified: None,
            permissions: None,
        })
    }

    async fn delete(&self, path: &str) -> Result<(), RemoteError> {
        let target = self.resolve_path(path);
        if target.is_dir() {
            fs::remove_dir_all(&target).await
                .map_err(|e| RemoteError::OperationFailed(format!("Failed to delete directory '{}': {}", path, e)))
        } else {
            fs::remove_file(&target).await
                .map_err(|e| RemoteError::OperationFailed(format!("Failed to delete file '{}': {}", path, e)))
        }
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), RemoteError> {
        let src = self.resolve_path(from);
        let dst = self.resolve_path(to);
        if let Some(parent) = dst.parent() {
            let _ = fs::create_dir_all(parent).await;
        }
        fs::rename(&src, &dst).await
            .map_err(|e| RemoteError::OperationFailed(format!("Failed to rename '{}' to '{}': {}", from, to, e)))
    }

    async fn create_file(&self, path: &str) -> Result<(), RemoteError> {
        self.write_file(path, &[]).await
    }

    async fn create_dir(&self, path: &str) -> Result<(), RemoteError> {
        let target = self.resolve_path(path);
        fs::create_dir_all(&target).await
            .map_err(|e| RemoteError::OperationFailed(format!("Failed to create directory '{}': {}", path, e)))
    }

    async fn search(&self, path: &str, query: &str, content_search: bool) -> Result<Vec<SearchMatch>, RemoteError> {
        let mut results = Vec::new();
        let query_lower = query.to_lowercase();
        let entries = self.list_dir(path).await?;

        for entry in entries {
            if entry.is_dir {
                if let Ok(mut sub_results) = Box::pin(self.search(&entry.path, query, content_search)).await {
                    results.append(&mut sub_results);
                }
            } else {
                if content_search {
                    if let Ok(bytes) = self.read_file(&entry.path).await {
                        if let Ok(text) = std::str::from_utf8(&bytes) {
                            for (idx, line) in text.lines().enumerate() {
                                if line.to_lowercase().contains(&query_lower) {
                                    results.push(SearchMatch {
                                        path: entry.path.clone(),
                                        line_number: Some(idx + 1),
                                        snippet: Some(line.trim().to_string()),
                                    });
                                }
                            }
                        }
                    }
                } else {
                    if entry.name.to_lowercase().contains(&query_lower) || entry.path.to_lowercase().contains(&query_lower) {
                        results.push(SearchMatch {
                            path: entry.path.clone(),
                            line_number: None,
                            snippet: None,
                        });
                    }
                }
            }
        }

        Ok(results)
    }
}
