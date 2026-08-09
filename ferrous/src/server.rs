use std::path::PathBuf;
use tokio::fs;
use ferrum_core::{
    Capability, FileEntry, FileMeta, FerrousRequest, FerrousResponse, SearchMatch,
};

pub struct FerrousServer;

impl FerrousServer {
    pub fn new() -> Self {
        Self
    }

    pub async fn handle_request(&self, req: FerrousRequest, capability: &Capability) -> FerrousResponse {
        match req {
            FerrousRequest::ListDir { path } => {
                if !capability.is_path_allowed(&path) {
                    return FerrousResponse::Forbidden { message: "Access denied by capability policy".into() };
                }
                match self.list_dir(&path).await {
                    Ok(entries) => FerrousResponse::ListDir { entries },
                    Err(e) => FerrousResponse::Error { message: e },
                }
            }
            FerrousRequest::ReadFile { path } => {
                if !capability.is_path_allowed(&path) {
                    return FerrousResponse::Forbidden { message: "Access denied by capability policy".into() };
                }
                match fs::read(&path).await {
                    Ok(data) => FerrousResponse::ReadFile { data },
                    Err(e) => FerrousResponse::Error { message: e.to_string() },
                }
            }
            FerrousRequest::WriteFile { path, content } => {
                if capability.read_only || !capability.is_path_allowed(&path) {
                    return FerrousResponse::Forbidden { message: "Permission denied or read-only mode".into() };
                }
                if let Some(parent) = PathBuf::from(&path).parent() {
                    let _ = fs::create_dir_all(parent).await;
                }
                match fs::write(&path, content).await {
                    Ok(_) => FerrousResponse::Success,
                    Err(e) => FerrousResponse::Error { message: e.to_string() },
                }
            }
            FerrousRequest::Stat { path } => {
                if !capability.is_path_allowed(&path) {
                    return FerrousResponse::Forbidden { message: "Access denied by capability policy".into() };
                }
                match fs::metadata(&path).await {
                    Ok(m) => FerrousResponse::Stat {
                        meta: FileMeta {
                            size: m.len(),
                            is_dir: m.is_dir(),
                            modified: None,
                            permissions: None,
                        },
                    },
                    Err(e) => FerrousResponse::Error { message: e.to_string() },
                }
            }
            FerrousRequest::Delete { path } => {
                if capability.read_only || !capability.is_path_allowed(&path) {
                    return FerrousResponse::Forbidden { message: "Permission denied or read-only mode".into() };
                }
                let target = PathBuf::from(&path);
                let res = if target.is_dir() {
                    fs::remove_dir_all(target).await
                } else {
                    fs::remove_file(target).await
                };
                match res {
                    Ok(_) => FerrousResponse::Success,
                    Err(e) => FerrousResponse::Error { message: e.to_string() },
                }
            }
            FerrousRequest::Rename { from, to } => {
                if capability.read_only || !capability.is_path_allowed(&from) || !capability.is_path_allowed(&to) {
                    return FerrousResponse::Forbidden { message: "Permission denied or read-only mode".into() };
                }
                match fs::rename(from, to).await {
                    Ok(_) => FerrousResponse::Success,
                    Err(e) => FerrousResponse::Error { message: e.to_string() },
                }
            }
            FerrousRequest::CreateFile { path } => {
                if capability.read_only || !capability.is_path_allowed(&path) {
                    return FerrousResponse::Forbidden { message: "Permission denied or read-only mode".into() };
                }
                match fs::write(&path, b"").await {
                    Ok(_) => FerrousResponse::Success,
                    Err(e) => FerrousResponse::Error { message: e.to_string() },
                }
            }
            FerrousRequest::CreateDir { path } => {
                if capability.read_only || !capability.is_path_allowed(&path) {
                    return FerrousResponse::Forbidden { message: "Permission denied or read-only mode".into() };
                }
                match fs::create_dir_all(&path).await {
                    Ok(_) => FerrousResponse::Success,
                    Err(e) => FerrousResponse::Error { message: e.to_string() },
                }
            }
            FerrousRequest::Search { path, query, content_search } => {
                if !capability.is_path_allowed(&path) {
                    return FerrousResponse::Forbidden { message: "Access denied by capability policy".into() };
                }
                match self.search_dir(&path, &query, content_search).await {
                    Ok(matches) => FerrousResponse::Search { matches },
                    Err(e) => FerrousResponse::Error { message: e },
                }
            }
            FerrousRequest::PtyOpen { .. } | FerrousRequest::PtyInput { .. } | FerrousRequest::PtyResize { .. } => {
                if !capability.allow_shell {
                    return FerrousResponse::Forbidden { message: "Shell access disabled by capability policy".into() };
                }
                FerrousResponse::Success
            }
        }
    }

    async fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>, String> {
        let mut dir = fs::read_dir(path).await.map_err(|e| e.to_string())?;
        let mut entries = Vec::new();
        while let Ok(Some(entry)) = dir.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            let meta = entry.metadata().await.ok();
            let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let full_path = entry.path().to_string_lossy().to_string();

            entries.push(FileEntry {
                name,
                path: full_path,
                is_dir,
                size,
                modified: None,
            });
        }
        Ok(entries)
    }

    async fn search_dir(&self, path: &str, query: &str, _content_search: bool) -> Result<Vec<SearchMatch>, String> {
        let mut matches = Vec::new();
        let mut dir = fs::read_dir(path).await.map_err(|e| e.to_string())?;
        while let Ok(Some(entry)) = dir.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.contains(query) {
                matches.push(SearchMatch {
                    path: entry.path().to_string_lossy().to_string(),
                    line_number: None,
                    snippet: None,
                });
            }
        }
        Ok(matches)
    }
}
