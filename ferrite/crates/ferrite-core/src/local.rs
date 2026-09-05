use std::path::PathBuf;

use async_trait::async_trait;
use futures_util::StreamExt;
use tokio::fs;
use tokio_util::io::ReaderStream;

use crate::config::RemoteConfig;
use crate::traits::{BoxStream, FileEntry, FileMeta, RemoteError, RemoteFilesystem, SearchMatch};
use ferrum_core::{modified_secs, normalize_path};

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

    /// Maps a browsed path (Ferrite's `/`-rooted virtual convention) onto a
    /// real path under `base_dir`.
    ///
    /// The request path is lexically normalized first, which clamps `..` at
    /// the virtual root. That is what keeps a remote configured with
    /// `base_path: /srv/share` confined to that directory: `/../../etc` maps
    /// to `<base>/etc`, never to `/etc`. Symlinks under the base are not
    /// resolved; a local remote is trusted to the extent its base is.
    fn resolve_path(&self, rel_path: &str) -> PathBuf {
        let normalized = normalize_path(rel_path);
        let clean = normalized.trim_start_matches('/');

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
                modified: metadata.as_ref().and_then(modified_secs),
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

    async fn write_stream(
        &self,
        path: &str,
        mut stream: BoxStream<'static, Result<bytes::Bytes, RemoteError>>,
    ) -> Result<(), RemoteError> {
        use tokio::io::AsyncWriteExt;
        let target = self.resolve_path(path);
        if let Some(parent) = target.parent() {
            let _ = fs::create_dir_all(parent).await;
        }
        let mut file = fs::File::create(&target).await
            .map_err(|e| RemoteError::OperationFailed(format!("Failed to write file '{}': {}", path, e)))?;
        while let Some(chunk) = stream.next().await {
            file.write_all(&chunk?).await
                .map_err(|e| RemoteError::OperationFailed(format!("Failed to write file '{}': {}", path, e)))?;
        }
        file.flush().await
            .map_err(|e| RemoteError::OperationFailed(format!("Failed to write file '{}': {}", path, e)))
    }

    async fn stat(&self, path: &str) -> Result<FileMeta, RemoteError> {
        let target = self.resolve_path(path);
        let meta = fs::metadata(&target).await
            .map_err(|e| RemoteError::NotFound(format!("Path not found '{}': {}", path, e)))?;
        Ok(FileMeta::from_metadata(&meta))
    }

    async fn delete(&self, path: &str) -> Result<(), RemoteError> {
        let target = self.resolve_path(path);
        // symlink_metadata so deleting a symlink removes the link, never the
        // directory it points at.
        let is_dir = fs::symlink_metadata(&target).await.map(|m| m.is_dir()).unwrap_or(false);
        if is_dir {
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
        let target = self.resolve_path(path);
        if let Some(parent) = target.parent() {
            let _ = fs::create_dir_all(parent).await;
        }
        // create_new: "new file" on an existing name must not truncate it.
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .await
            .map(|_| ())
            .map_err(|e| RemoteError::OperationFailed(format!("Failed to create file '{}': {}", path, e)))
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
            } else if content_search {
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
            } else if entry.name.to_lowercase().contains(&query_lower) || entry.path.to_lowercase().contains(&query_lower) {
                results.push(SearchMatch {
                    path: entry.path.clone(),
                    line_number: None,
                    snippet: None,
                });
            }
        }

        Ok(results)
    }

    fn local_path(&self, path: &str) -> Option<PathBuf> {
        Some(self.resolve_path(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthConfig, ProtocolType};

    fn backend(base: Option<&str>) -> LocalBackend {
        LocalBackend::new(RemoteConfig {
            name: "local".into(),
            protocol: ProtocolType::Local,
            host: "localhost".into(),
            port: 0,
            username: "local".into(),
            auth: AuthConfig {
                password: None,
                private_key: None,
                private_key_path: None,
                passphrase: None,
                secret_key: None,
            },
            bucket: None,
            region: None,
            endpoint: None,
            use_ssl: None,
            base_path: base.map(|s| s.to_string()),
            agent_pubkey: None,
        })
    }

    #[test]
    fn base_path_confines_traversal() {
        let b = backend(Some("/srv/share"));
        let base = PathBuf::from("/srv/share");
        assert_eq!(b.resolve_path("/docs/a.txt"), base.join("docs/a.txt"));
        assert_eq!(b.resolve_path("/../../etc/passwd"), base.join("etc/passwd"));
        assert_eq!(b.resolve_path("/docs/../../../etc/passwd"), base.join("etc/passwd"));
        assert_eq!(b.resolve_path("/"), base);
        assert_eq!(b.resolve_path(""), base);
    }

    #[test]
    fn local_path_is_the_resolved_path() {
        let b = backend(Some("/srv/share"));
        assert_eq!(b.local_path("/x/y"), Some(PathBuf::from("/srv/share").join("x/y")));
    }

    #[cfg(windows)]
    #[test]
    fn drive_root_mode_maps_drive_letters() {
        let b = backend(None);
        assert_eq!(b.resolve_path("/C:"), PathBuf::from("C:\\"));
        assert_eq!(b.resolve_path("/C:/Users/../Windows"), PathBuf::from("C:/Windows"));
        assert_eq!(b.resolve_path(""), PathBuf::new());
    }
}
