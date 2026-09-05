//! Git integration for the local remote. Every invocation shells out to
//! `git` with an explicit argument list (never a shell), user-supplied
//! names after `--` where git accepts it, and a refusal for anything that
//! looks like an option where it does not.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{extract::{Query, State}, Json};
use serde::{Deserialize, Serialize};
use tower_cookies::Cookies;

use super::{invalid, local_disk_path, ApiError, AppState};
use crate::permissions::authorize;
use ferrite_core::RemoteError;

const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct GitQueryParams {
    pub remote: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct GitFileQueryParams {
    pub remote: String,
    pub path: String,
    pub file: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct GitBlameQueryParams {
    pub remote: String,
    pub path: String,
    pub file: String,
    pub line: usize,
}

#[derive(Debug, Serialize)]
pub struct FileGitStatus {
    pub path: String,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct GitStatusResponse {
    pub is_git: bool,
    pub branch: String,
    pub repo_root: String,
    pub statuses: Vec<FileGitStatus>,
}

#[derive(Debug, Serialize)]
pub struct GitDiffResponse {
    pub path: String,
    pub original: String,
    pub modified: String,
}

#[derive(Debug, Serialize)]
pub struct GitBlameResponse {
    pub line: usize,
    pub author: String,
    pub time_ago: String,
    pub summary: String,
    pub hash: String,
}

#[derive(Debug, Serialize)]
pub struct GitCommitItem {
    pub hash: String,
    pub author: String,
    pub time_ago: String,
    pub summary: String,
}

#[derive(Debug, Serialize)]
pub struct GitHistoryResponse {
    pub path: String,
    pub commits: Vec<GitCommitItem>,
}

#[derive(Debug, Deserialize)]
pub struct GitCommitPayload {
    pub path: String,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct GitStagePayload {
    pub path: String,
    pub file: String,
}

#[derive(Debug, Deserialize)]
pub struct GitCheckoutPayload {
    pub path: String,
    pub branch: String,
}

#[derive(Debug, Serialize)]
pub struct GitBranchItem {
    pub name: String,
    pub is_current: bool,
    pub is_remote: bool,
}

#[derive(Debug, Serialize)]
pub struct GitBranchesResponse {
    pub current: String,
    pub branches: Vec<GitBranchItem>,
}

/// The directory to run git in for a browsed path: the path itself for a
/// directory, its parent for a file (or something that looks like one).
fn repo_dir_for(state: &AppState, path: &str) -> Result<PathBuf, ApiError> {
    let p = local_disk_path(state, path)?;
    let text = p.to_string_lossy();
    let looks_like_file = p.is_file()
        || (text.contains('.') && !text.ends_with('/') && !text.ends_with('\\') && p.extension().is_some());
    Ok(if looks_like_file {
        p.parent().map(Path::to_path_buf).unwrap_or(p)
    } else {
        p
    })
}

/// A file path inside the repository, as git wants it. Rejects anything
/// that git could read as an option and anything that leaves the repo.
fn repo_relative(file: &str) -> Result<String, ApiError> {
    let clean = file.replace('\\', "/");
    if clean.is_empty() || clean.starts_with('-') || clean.starts_with('/') {
        return Err(invalid("Invalid file path"));
    }
    // Anchor under a sentinel root and normalize: anything that climbs out
    // of the sentinel would climb out of the repository too.
    let anchored = ferrum_core::normalize_path(&format!("/repo/{}", clean));
    if !anchored.starts_with("/repo/") {
        return Err(invalid("File path may not leave the repository"));
    }
    Ok(clean)
}

fn check_branch_name(name: &str) -> Result<(), ApiError> {
    let ok = !name.is_empty()
        && !name.starts_with('-')
        && !name.contains("..")
        && name.chars().all(|c| !c.is_control() && !c.is_whitespace() && !matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\'));
    if ok {
        Ok(())
    } else {
        Err(invalid("Invalid branch name"))
    }
}

async fn git(dir: &Path, args: &[&str]) -> Result<std::process::Output, ApiError> {
    let out = tokio::time::timeout(
        GIT_TIMEOUT,
        tokio::process::Command::new("git").args(args).current_dir(dir).output(),
    )
    .await
    .map_err(|_| ApiError(RemoteError::ConnectionFailed("git timed out".into())))?
    .map_err(|e| ApiError(RemoteError::ConnectionFailed(format!("git failed to start: {}", e))))?;
    Ok(out)
}

/// Runs git and returns stdout on success, or the command's stderr as an
/// error the client can show.
async fn git_ok(dir: &Path, args: &[&str]) -> Result<String, ApiError> {
    let out = git(dir, args).await?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(ApiError(RemoteError::ConnectionFailed(String::from_utf8_lossy(&out.stderr).trim().to_string())))
    }
}

fn time_ago(unix_secs: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let diff = now.saturating_sub(unix_secs);
    if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86400 {
        format!("{}h ago", diff / 3600)
    } else {
        format!("{}d ago", diff / 86400)
    }
}

pub async fn git_status(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<GitQueryParams>,
) -> Result<Json<GitStatusResponse>, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, "local", &query.path, false)?;
    let repo_dir = repo_dir_for(&state, &query.path)?;

    let not_git = || GitStatusResponse { is_git: false, branch: String::new(), repo_root: String::new(), statuses: vec![] };
    let Ok(toplevel) = git_ok(&repo_dir, &["rev-parse", "--show-toplevel"]).await else {
        return Ok(Json(not_git()));
    };
    let repo_root = toplevel.trim().replace('\\', "/");
    if repo_root.is_empty() {
        return Ok(Json(not_git()));
    }
    let root = Path::new(&repo_root);

    let branch = git_ok(root, &["branch", "--show-current"])
        .await
        .map(|b| b.trim().to_string())
        .ok()
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "HEAD".to_string());

    let mut statuses = Vec::new();
    if let Ok(text) = git_ok(root, &["status", "--porcelain=v1"]).await {
        for line in text.lines().filter(|l| l.len() >= 4) {
            let code = line[0..2].trim();
            let file_path = line[3..].trim().replace('\\', "/");
            let status = ['?', 'M', 'D', 'A']
                .into_iter()
                .find(|c| code.contains(*c))
                .map(|c| if c == '?' { "U".to_string() } else { c.to_string() })
                .unwrap_or_else(|| code.to_string());
            statuses.push(FileGitStatus { path: file_path, status });
        }
    }

    Ok(Json(GitStatusResponse { is_git: true, branch, repo_root, statuses }))
}

pub async fn git_diff(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<GitFileQueryParams>,
) -> Result<Json<GitDiffResponse>, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, "local", &query.path, false)?;
    let repo_dir = repo_dir_for(&state, &query.path)?;
    let file = repo_relative(&query.file)?;

    let original = git_ok(&repo_dir, &["show", &format!("HEAD:{}", file)]).await.unwrap_or_default();
    let modified = tokio::fs::read_to_string(repo_dir.join(&file)).await.unwrap_or_default();

    Ok(Json(GitDiffResponse { path: query.file, original, modified }))
}

pub async fn git_blame(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<GitBlameQueryParams>,
) -> Result<Json<GitBlameResponse>, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, "local", &query.path, false)?;
    let repo_dir = repo_dir_for(&state, &query.path)?;
    let file = repo_relative(&query.file)?;
    let line_spec = format!("{},{}", query.line, query.line);

    let mut resp = GitBlameResponse {
        line: query.line,
        author: "Not Committed Yet".to_string(),
        time_ago: "now".to_string(),
        summary: "Uncommitted changes".to_string(),
        hash: "0000000".to_string(),
    };

    if let Ok(text) = git_ok(&repo_dir, &["blame", "-L", &line_spec, "--porcelain", "--", &file]).await {
        let mut lines = text.lines();
        if let Some(first) = lines.next() {
            if let Some(hash) = first.split_whitespace().next() {
                resp.hash = hash.chars().take(7).collect();
            }
        }
        for line in lines {
            if let Some(rest) = line.strip_prefix("author ") {
                resp.author = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("summary ") {
                resp.summary = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("author-time ") {
                if let Ok(ts) = rest.trim().parse::<u64>() {
                    resp.time_ago = time_ago(ts);
                }
            }
        }
    }

    Ok(Json(resp))
}

pub async fn git_history(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<GitFileQueryParams>,
) -> Result<Json<GitHistoryResponse>, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, "local", &query.path, false)?;
    let repo_dir = repo_dir_for(&state, &query.path)?;
    let file = repo_relative(&query.file)?;

    let mut commits = Vec::new();
    if let Ok(text) = git_ok(&repo_dir, &["log", "-n", "15", "--pretty=format:%h|%an|%cr|%s", "--", &file]).await {
        for line in text.lines() {
            let parts: Vec<&str> = line.splitn(4, '|').collect();
            if parts.len() == 4 {
                commits.push(GitCommitItem {
                    hash: parts[0].to_string(),
                    author: parts[1].to_string(),
                    time_ago: parts[2].to_string(),
                    summary: parts[3].to_string(),
                });
            }
        }
    }

    Ok(Json(GitHistoryResponse { path: query.file, commits }))
}

pub async fn git_commit(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Json(payload): Json<GitCommitPayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, "local", &payload.path, true)?;
    let repo_dir = repo_dir_for(&state, &payload.path)?;
    let message = payload.message.trim();
    if message.is_empty() {
        return Err(invalid("Commit message is required"));
    }

    git_ok(&repo_dir, &["add", "-A"]).await?;
    let output = git_ok(&repo_dir, &["commit", "-m", message]).await?;
    Ok(Json(serde_json::json!({ "success": true, "output": output })))
}

pub async fn git_stage(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Json(payload): Json<GitStagePayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, "local", &payload.path, true)?;
    let repo_dir = repo_dir_for(&state, &payload.path)?;
    let file = repo_relative(&payload.file)?;
    git_ok(&repo_dir, &["add", "--", &file]).await?;
    Ok(Json(serde_json::json!({ "success": true })))
}

pub async fn git_unstage(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Json(payload): Json<GitStagePayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, "local", &payload.path, true)?;
    let repo_dir = repo_dir_for(&state, &payload.path)?;
    let file = repo_relative(&payload.file)?;
    git_ok(&repo_dir, &["restore", "--staged", "--", &file]).await?;
    Ok(Json(serde_json::json!({ "success": true })))
}

pub async fn git_revert(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Json(payload): Json<GitStagePayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, "local", &payload.path, true)?;
    let repo_dir = repo_dir_for(&state, &payload.path)?;
    let file = repo_relative(&payload.file)?;
    git_ok(&repo_dir, &["checkout", "--", &file]).await?;
    Ok(Json(serde_json::json!({ "success": true })))
}

pub async fn git_branches(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<GitQueryParams>,
) -> Result<Json<GitBranchesResponse>, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, "local", &query.path, false)?;
    let repo_dir = repo_dir_for(&state, &query.path)?;

    let mut current = "HEAD".to_string();
    let mut branches = Vec::new();
    if let Ok(text) = git_ok(&repo_dir, &["branch", "-a"]).await {
        for line in text.lines() {
            let trimmed = line.trim();
            let is_current = trimmed.starts_with('*');
            let name = trimmed.trim_start_matches('*').trim().to_string();
            if is_current {
                current = name.clone();
            }
            if !name.contains("->") && !name.is_empty() {
                let is_remote = name.starts_with("remotes/");
                branches.push(GitBranchItem { name, is_current, is_remote });
            }
        }
    }

    Ok(Json(GitBranchesResponse { current, branches }))
}

pub async fn git_checkout(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Json(payload): Json<GitCheckoutPayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, "local", &payload.path, true)?;
    let repo_dir = repo_dir_for(&state, &payload.path)?;
    check_branch_name(&payload.branch)?;
    git_ok(&repo_dir, &["checkout", &payload.branch]).await?;
    Ok(Json(serde_json::json!({ "success": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_like_and_escaping_paths_are_refused() {
        assert!(repo_relative("src/main.rs").is_ok());
        assert!(repo_relative("-oProxyCommand=x").is_err());
        assert!(repo_relative("../outside").is_err());
        assert!(repo_relative("a/../../b").is_err());
        assert!(repo_relative("a/../b").is_ok());
        assert!(repo_relative("/etc/passwd").is_err());
        assert!(repo_relative("").is_err());
    }

    #[test]
    fn branch_names_follow_git_rules() {
        assert!(check_branch_name("main").is_ok());
        assert!(check_branch_name("feature/x-1").is_ok());
        assert!(check_branch_name("--orphan").is_err());
        assert!(check_branch_name("a..b").is_err());
        assert!(check_branch_name("has space").is_err());
        assert!(check_branch_name("").is_err());
    }
}
