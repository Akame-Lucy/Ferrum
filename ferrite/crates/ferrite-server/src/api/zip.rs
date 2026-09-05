//! Looking inside archives on the local remote without extracting them.

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{header, HeaderMap, HeaderValue},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use tower_cookies::Cookies;

use super::{invalid, local_disk_path, ApiError, AppState, FileQuery};
use crate::permissions::authorize;
use ferrite_core::RemoteError;

/// An entry larger than this is not previewed inline. The declared size is
/// checked before a byte is inflated, so a zip bomb never gets to expand.
const MAX_EXTRACT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_LISTED_ENTRIES: usize = 20_000;

#[derive(Debug, Serialize)]
pub struct ZipEntryItem {
    pub name: String,
    pub uncompressed_size: u64,
    pub compressed_size: u64,
    pub is_dir: bool,
}

#[derive(Debug, Serialize)]
pub struct ZipInspectResponse {
    pub path: String,
    pub entries: Vec<ZipEntryItem>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ZipExtractQuery {
    pub remote: String,
    pub path: String,
    pub entry: String,
}

fn open_archive(state: &AppState, path: &str) -> Result<zip::ZipArchive<std::fs::File>, ApiError> {
    let p = local_disk_path(state, path)?;
    let file = std::fs::File::open(&p).map_err(|e| ApiError(RemoteError::NotFound(e.to_string())))?;
    zip::ZipArchive::new(file).map_err(|e| invalid(format!("Not a readable zip archive: {}", e)))
}

pub async fn zip_inspect(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<FileQuery>,
) -> Result<Json<ZipInspectResponse>, ApiError> {
    let user = state.current_user(&cookies)?;
    // Reads directly off the server's local disk regardless of `query.remote`.
    authorize(&user, "local", &query.path, false)?;
    let mut archive = open_archive(&state, &query.path)?;

    let mut entries = Vec::new();
    for i in 0..archive.len().min(MAX_LISTED_ENTRIES) {
        if let Ok(entry) = archive.by_index(i) {
            entries.push(ZipEntryItem {
                name: entry.name().to_string(),
                uncompressed_size: entry.size(),
                compressed_size: entry.compressed_size(),
                is_dir: entry.is_dir(),
            });
        }
    }

    Ok(Json(ZipInspectResponse { path: query.path, entries }))
}

pub async fn zip_extract_entry(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<ZipExtractQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, "local", &query.path, false)?;
    let mut archive = open_archive(&state, &query.path)?;

    let mut entry = archive.by_name(&query.entry).map_err(|e| ApiError(RemoteError::NotFound(e.to_string())))?;
    if entry.is_dir() {
        return Err(invalid("Entry is a directory"));
    }
    if entry.size() > MAX_EXTRACT_BYTES {
        return Err(invalid(format!(
            "Entry is {} bytes; inline preview is limited to {} bytes",
            entry.size(),
            MAX_EXTRACT_BYTES
        )));
    }

    // The declared size is what the header claims; `take` makes sure a lying
    // header cannot inflate past the limit either.
    let mut buffer = Vec::with_capacity(entry.size() as usize);
    std::io::Read::read_to_end(&mut std::io::Read::take(&mut entry, MAX_EXTRACT_BYTES + 1), &mut buffer)
        .map_err(|e| ApiError(RemoteError::OperationFailed(e.to_string())))?;
    if buffer.len() as u64 > MAX_EXTRACT_BYTES {
        return Err(invalid("Entry inflated past the preview limit"));
    }

    let mime = mime_guess::from_path(&query.entry).first_or_octet_stream().to_string();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        mime.parse().unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));

    Ok((headers, Bytes::from(buffer)))
}
