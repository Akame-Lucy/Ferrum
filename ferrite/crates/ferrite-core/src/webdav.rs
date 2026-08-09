use async_trait::async_trait;
use reqwest::{blocking::Client, Method};
use xmltree::Element;

use crate::config::RemoteConfig;
use crate::traits::{FileEntry, FileMeta, RemoteError, RemoteFilesystem, SearchMatch};

pub struct WebDavBackend {
    config: RemoteConfig,
}

impl WebDavBackend {
    pub fn new(config: RemoteConfig) -> Self {
        Self { config }
    }

    fn base_url(&self) -> String {
        if let Some(ref ep) = self.config.endpoint {
            ep.trim_end_matches('/').to_string()
        } else {
            let scheme = if self.config.use_ssl.unwrap_or(true) { "https" } else { "http" };
            let port = if self.config.port == 22 { 443 } else { self.config.port };
            format!("{}://{}:{}", scheme, self.config.host, port)
        }
    }

    fn build_url(&self, path: &str) -> String {
        let base = self.base_url();
        let p = path.trim_start_matches('/');
        if p.is_empty() {
            format!("{}/", base)
        } else {
            format!("{}/{}", base, p)
        }
    }

    fn create_client() -> Client {
        Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap_or_default()
    }
}

#[async_trait]
impl RemoteFilesystem for WebDavBackend {
    async fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>, RemoteError> {
        let backend_url = self.build_url(path);
        let username = self.config.username.clone();
        let password = self.config.auth.password.clone().unwrap_or_default();
        let target_path = path.to_string();

        tokio::task::spawn_blocking(move || {
            let client = Self::create_client();
            let propfind_body = r#"<?xml version="1.0" encoding="utf-8" ?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:displayname/>
    <D:resourcetype/>
    <D:getcontentlength/>
    <D:getlastmodified/>
  </D:prop>
</D:propfind>"#;

            let method = Method::from_bytes(b"PROPFIND").map_err(|e| RemoteError::OperationFailed(e.to_string()))?;
            let mut req = client.request(method, &backend_url)
                .header("Depth", "1")
                .header("Content-Type", "application/xml")
                .body(propfind_body);

            if !username.is_empty() {
                req = req.basic_auth(&username, Some(&password));
            }

            let res = req.send()
                .map_err(|e| RemoteError::ConnectionFailed(format!("WebDAV request failed: {}", e)))?;

            if !res.status().is_success() && res.status().as_u16() != 207 {
                return Err(RemoteError::OperationFailed(format!("WebDAV PROPFIND failed with status: {}", res.status())));
            }

            let xml_text = res.text().map_err(|e| RemoteError::Io(std::io::Error::other(e)))?;
            let root = Element::parse(xml_text.as_bytes())
                .map_err(|e| RemoteError::OperationFailed(format!("Failed to parse WebDAV XML: {}", e)))?;

            let mut entries = Vec::new();
            for response in root.children.iter().filter_map(|c| c.as_element()) {
                if response.name.ends_with("response") {
                    let href_text = response.get_child("href").and_then(|h| h.get_text());
                    let href = href_text.as_deref().unwrap_or_default();
                    let propstat = response.get_child("propstat").and_then(|p| p.get_child("prop"));
                    let is_dir = propstat.and_then(|p| p.get_child("resourcetype")).map(|r| r.get_child("collection").is_some()).unwrap_or(false);
                    let size_text = propstat.and_then(|p| p.get_child("getcontentlength")).and_then(|s| s.get_text());
                    let size = size_text.as_deref().and_then(|t| t.parse::<u64>().ok()).unwrap_or(0);

                    let clean_href = href.trim_end_matches('/');
                    let name = clean_href.split('/').next_back().unwrap_or(clean_href).to_string();
                    if name.is_empty() || name == target_path.trim_end_matches('/') {
                        continue;
                    }

                    let full_path = if target_path.ends_with('/') {
                        format!("{}{}", target_path, name)
                    } else {
                        format!("{}/{}", target_path, name)
                    };

                    entries.push(FileEntry {
                        name,
                        path: full_path,
                        is_dir,
                        size,
                        modified: None,
                    });
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
        }).await.map_err(|e| RemoteError::OperationFailed(e.to_string()))?
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, RemoteError> {
        let backend_url = self.build_url(path);
        let username = self.config.username.clone();
        let password = self.config.auth.password.clone().unwrap_or_default();
        let target_path = path.to_string();

        tokio::task::spawn_blocking(move || {
            let client = Self::create_client();
            let mut req = client.get(&backend_url);
            if !username.is_empty() {
                req = req.basic_auth(&username, Some(&password));
            }

            let res = req.send()
                .map_err(|e| RemoteError::ConnectionFailed(format!("Failed to GET WebDAV file: {}", e)))?;

            if !res.status().is_success() {
                return Err(RemoteError::NotFound(format!("File not found on WebDAV '{}': {}", target_path, res.status())));
            }

            let bytes = res.bytes()
                .map_err(|e| RemoteError::Io(std::io::Error::other(e)))?;

            Ok(bytes.to_vec())
        }).await.map_err(|e| RemoteError::OperationFailed(e.to_string()))?
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), RemoteError> {
        let backend_url = self.build_url(path);
        let username = self.config.username.clone();
        let password = self.config.auth.password.clone().unwrap_or_default();
        let data = contents.to_vec();

        tokio::task::spawn_blocking(move || {
            let client = Self::create_client();
            let mut req = client.put(&backend_url).body(data);
            if !username.is_empty() {
                req = req.basic_auth(&username, Some(&password));
            }

            let res = req.send()
                .map_err(|e| RemoteError::ConnectionFailed(format!("Failed to PUT WebDAV file: {}", e)))?;

            if !res.status().is_success() {
                return Err(RemoteError::OperationFailed(format!("WebDAV PUT failed with status: {}", res.status())));
            }

            Ok(())
        }).await.map_err(|e| RemoteError::OperationFailed(e.to_string()))?
    }

    async fn stat(&self, path: &str) -> Result<FileMeta, RemoteError> {
        let entries = self.list_dir(path).await?;
        let is_dir = path.ends_with('/') || !entries.is_empty();
        Ok(FileMeta {
            size: 0,
            is_dir,
            modified: None,
            permissions: None,
        })
    }

    async fn delete(&self, path: &str) -> Result<(), RemoteError> {
        let backend_url = self.build_url(path);
        let username = self.config.username.clone();
        let password = self.config.auth.password.clone().unwrap_or_default();

        tokio::task::spawn_blocking(move || {
            let client = Self::create_client();
            let mut req = client.delete(&backend_url);
            if !username.is_empty() {
                req = req.basic_auth(&username, Some(&password));
            }

            let res = req.send()
                .map_err(|e| RemoteError::ConnectionFailed(format!("Failed to DELETE WebDAV path: {}", e)))?;

            if !res.status().is_success() {
                return Err(RemoteError::OperationFailed(format!("WebDAV DELETE failed with status: {}", res.status())));
            }

            Ok(())
        }).await.map_err(|e| RemoteError::OperationFailed(e.to_string()))?
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), RemoteError> {
        let src_url = self.build_url(from);
        let dst_url = self.build_url(to);
        let username = self.config.username.clone();
        let password = self.config.auth.password.clone().unwrap_or_default();

        tokio::task::spawn_blocking(move || {
            let client = Self::create_client();
            let method = Method::from_bytes(b"MOVE").map_err(|e| RemoteError::OperationFailed(e.to_string()))?;
            let mut req = client.request(method, &src_url)
                .header("Destination", dst_url);

            if !username.is_empty() {
                req = req.basic_auth(&username, Some(&password));
            }

            let res = req.send()
                .map_err(|e| RemoteError::ConnectionFailed(format!("Failed to MOVE WebDAV path: {}", e)))?;

            if !res.status().is_success() {
                return Err(RemoteError::OperationFailed(format!("WebDAV MOVE failed with status: {}", res.status())));
            }

            Ok(())
        }).await.map_err(|e| RemoteError::OperationFailed(e.to_string()))?
    }

    async fn create_file(&self, path: &str) -> Result<(), RemoteError> {
        self.write_file(path, &[]).await
    }

    async fn create_dir(&self, path: &str) -> Result<(), RemoteError> {
        let backend_url = self.build_url(path);
        let username = self.config.username.clone();
        let password = self.config.auth.password.clone().unwrap_or_default();

        tokio::task::spawn_blocking(move || {
            let client = Self::create_client();
            let method = Method::from_bytes(b"MKCOL").map_err(|e| RemoteError::OperationFailed(e.to_string()))?;
            let mut req = client.request(method, &backend_url);
            if !username.is_empty() {
                req = req.basic_auth(&username, Some(&password));
            }

            let res = req.send()
                .map_err(|e| RemoteError::ConnectionFailed(format!("Failed to MKCOL WebDAV directory: {}", e)))?;

            if !res.status().is_success() {
                return Err(RemoteError::OperationFailed(format!("WebDAV MKCOL failed with status: {}", res.status())));
            }

            Ok(())
        }).await.map_err(|e| RemoteError::OperationFailed(e.to_string()))?
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
