use async_trait::async_trait;
use s3::bucket::Bucket;
use s3::creds::Credentials;
use s3::region::Region;

use crate::config::RemoteConfig;
use crate::traits::{FileEntry, FileMeta, RemoteError, RemoteFilesystem, SearchMatch};

pub struct S3Backend {
    config: RemoteConfig,
}

impl S3Backend {
    pub fn new(config: RemoteConfig) -> Self {
        Self { config }
    }

    fn create_bucket(&self) -> Result<Bucket, RemoteError> {
        let bucket_name = self.config.bucket.as_deref().unwrap_or("default");
        let access_key = &self.config.username;
        let secret_key = self.config.auth.password.as_deref()
            .or(self.config.auth.secret_key.as_deref())
            .unwrap_or_default();

        let creds = Credentials::new(Some(access_key), Some(secret_key), None, None, None)
            .map_err(|e| RemoteError::AuthFailed(format!("Failed to create S3 credentials: {}", e)))?;

        let region = if let Some(ref ep) = self.config.endpoint {
            Region::Custom {
                region: self.config.region.clone().unwrap_or_else(|| "us-east-1".to_string()),
                endpoint: ep.clone(),
            }
        } else {
            self.config.region.as_deref().unwrap_or("us-east-1").parse()
                .unwrap_or(Region::UsEast1)
        };

        let mut bucket = Bucket::new(bucket_name, region, creds)
            .map_err(|e| RemoteError::ConnectionFailed(format!("Failed to initialize S3 bucket: {}", e)))?;

        if self.config.endpoint.is_some() {
            bucket.set_path_style();
        }

        Ok(bucket)
    }
}

#[async_trait]
impl RemoteFilesystem for S3Backend {
    async fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>, RemoteError> {
        let bucket = self.create_bucket()?;
        let target_path = path.trim_start_matches('/').to_string();

        let prefix = if target_path.is_empty() {
            String::new()
        } else if target_path.ends_with('/') {
            target_path.clone()
        } else {
            format!("{}/", target_path)
        };

        let list_result = bucket.list(prefix.clone(), Some("/".to_string())).await
            .map_err(|e| RemoteError::OperationFailed(format!("Failed to list S3 prefix '{}': {}", prefix, e)))?;

        let mut entries = Vec::new();

        if let Some(first) = list_result.first() {
            for object in &first.contents {
                let key = &object.key;
                if key == &prefix || key.is_empty() {
                    continue;
                }
                let name = key.strip_prefix(&prefix).unwrap_or(key).to_string();
                if name.is_empty() {
                    continue;
                }
                let is_dir = name.ends_with('/');
                let clean_name = name.trim_end_matches('/').to_string();

                entries.push(FileEntry {
                    name: clean_name.clone(),
                    path: format!("{}{}", prefix, name),
                    is_dir,
                    size: object.size,
                    modified: None,
                });
            }

            if let Some(ref prefixes) = first.common_prefixes {
                for cp in prefixes {
                    let prefix_str = &cp.prefix;
                    let name = prefix_str.strip_prefix(&prefix).unwrap_or(prefix_str).trim_end_matches('/').to_string();
                    if !name.is_empty() {
                        entries.push(FileEntry {
                            name,
                            path: prefix_str.clone(),
                            is_dir: true,
                            size: 0,
                            modified: None,
                        });
                    }
                }
            }
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
        let bucket = self.create_bucket()?;
        let target_path = path.trim_start_matches('/').to_string();

        let response = bucket.get_object(&target_path).await
            .map_err(|e| RemoteError::NotFound(format!("Failed to read S3 object '{}': {}", target_path, e)))?;

        Ok(response.bytes().to_vec())
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), RemoteError> {
        let bucket = self.create_bucket()?;
        let target_path = path.trim_start_matches('/').to_string();

        bucket.put_object(&target_path, contents).await
            .map_err(|e| RemoteError::OperationFailed(format!("Failed to write S3 object '{}': {}", target_path, e)))?;

        Ok(())
    }

    async fn stat(&self, path: &str) -> Result<FileMeta, RemoteError> {
        let bucket = self.create_bucket()?;
        let target_path = path.trim_start_matches('/').to_string();

        let (head, _) = bucket.head_object(&target_path).await
            .map_err(|e| RemoteError::NotFound(format!("Failed to stat S3 object '{}': {}", target_path, e)))?;

        Ok(FileMeta {
            size: head.content_length.unwrap_or(0) as u64,
            is_dir: target_path.ends_with('/'),
            modified: None,
            permissions: None,
        })
    }

    async fn delete(&self, path: &str) -> Result<(), RemoteError> {
        let bucket = self.create_bucket()?;
        let target_path = path.trim_start_matches('/').to_string();

        bucket.delete_object(&target_path).await
            .map_err(|e| RemoteError::OperationFailed(format!("Failed to delete S3 object '{}': {}", target_path, e)))?;

        Ok(())
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), RemoteError> {
        let bucket = self.create_bucket()?;
        let src_path = from.trim_start_matches('/').to_string();
        let dst_path = to.trim_start_matches('/').to_string();

        bucket.copy_object_internal(&src_path, &dst_path).await
            .map_err(|e| RemoteError::OperationFailed(format!("Failed to copy S3 object from '{}' to '{}': {}", src_path, dst_path, e)))?;
        bucket.delete_object(&src_path).await
            .map_err(|e| RemoteError::OperationFailed(format!("Failed to delete original S3 object '{}': {}", src_path, e)))?;

        Ok(())
    }

    async fn create_file(&self, path: &str) -> Result<(), RemoteError> {
        self.write_file(path, &[]).await
    }

    async fn create_dir(&self, path: &str) -> Result<(), RemoteError> {
        let dir_key = if path.ends_with('/') {
            path.to_string()
        } else {
            format!("{}/", path)
        };
        self.write_file(&dir_key, &[]).await
    }

    async fn search(&self, path: &str, query: &str, content_search: bool) -> Result<Vec<SearchMatch>, RemoteError> {
        let bucket = self.create_bucket()?;
        let target_prefix = path.trim_start_matches('/').to_string();
        let query_lower = query.to_lowercase();

        let list_result = bucket.list(target_prefix.clone(), None).await
            .map_err(|e| RemoteError::OperationFailed(format!("Failed to list S3 for search: {}", e)))?;

        let mut results = Vec::new();

        if let Some(first) = list_result.first() {
            for object in &first.contents {
                let key = &object.key;
                if key.ends_with('/') {
                    continue;
                }

                if content_search {
                    if let Ok(response) = bucket.get_object(key).await {
                        if let Ok(text) = std::str::from_utf8(response.bytes()) {
                            for (idx, line) in text.lines().enumerate() {
                                if line.to_lowercase().contains(&query_lower) {
                                    results.push(SearchMatch {
                                        path: key.clone(),
                                        line_number: Some(idx + 1),
                                        snippet: Some(line.trim().to_string()),
                                    });
                                }
                            }
                        }
                    }
                } else {
                    if key.to_lowercase().contains(&query_lower) {
                        results.push(SearchMatch {
                            path: key.clone(),
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
