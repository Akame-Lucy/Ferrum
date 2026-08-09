use serde::{Deserialize, Serialize};
use crate::traits::{FileEntry, FileMeta, SearchMatch};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    pub allowed_paths: Vec<String>,
    pub read_only: bool,
    pub allow_shell: bool,
}

impl Default for Capability {
    fn default() -> Self {
        Self {
            allowed_paths: vec!["/".to_string(), "C:\\".to_string()],
            read_only: false,
            allow_shell: true,
        }
    }
}

fn normalize_capability_path(p: &str) -> String {
    let s = p.replace('\\', "/");
    if s.is_empty() {
        return "/".to_string();
    }
    let trimmed = s.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

impl Capability {
    /// Requires a `/`-boundary or exact match, so an allowed path of `/foo`
    /// does not also cover `/foobar`.
    pub fn is_path_allowed(&self, path: &str) -> bool {
        if self.allowed_paths.is_empty() {
            return true;
        }
        let normalized = normalize_capability_path(path);
        self.allowed_paths.iter().any(|allowed| {
            let allowed_norm = normalize_capability_path(allowed);
            if allowed_norm == "/" {
                return true;
            }
            normalized == allowed_norm || normalized.starts_with(&format!("{}/", allowed_norm))
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum FerrousRequest {
    ListDir { path: String },
    ReadFile { path: String },
    WriteFile { path: String, content: Vec<u8> },
    Stat { path: String },
    Delete { path: String },
    Rename { from: String, to: String },
    CreateFile { path: String },
    CreateDir { path: String },
    Search { path: String, query: String, content_search: bool },
    PtyOpen { cols: u32, rows: u32 },
    PtyInput { data: Vec<u8> },
    PtyResize { cols: u32, rows: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum FerrousResponse {
    ListDir { entries: Vec<FileEntry> },
    ReadFile { data: Vec<u8> },
    Stat { meta: FileMeta },
    Search { matches: Vec<SearchMatch> },
    PtyOutput { data: Vec<u8> },
    Success,
    Error { message: String },
    /// Distinct from `Error`: the request was well-formed but the client's
    /// capability doesn't cover it, so callers can surface a clean "access
    /// denied" instead of a generic failure.
    Forbidden { message: String },
}
