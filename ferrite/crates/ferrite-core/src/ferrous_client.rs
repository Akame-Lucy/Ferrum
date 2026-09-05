use async_trait::async_trait;
use bytes::Bytes;
use ferrum_core::{
    FerrousRequest, FerrousResponse, FileEntry, FileMeta, NoiseSession, RemoteError,
    RemoteFilesystem, SearchMatch, TRANSFER_CHUNK,
};
use futures_util::StreamExt;
use snow::Keypair;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::traits::BoxStream;

/// How long to wait for the TCP connect and the Noise handshake together. A
/// host that is down should fail the UI's request promptly, not hang it.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

pub struct FerrousBackend {
    pub host: String,
    pub port: u16,
    pub client_keypair: Arc<Keypair>,
    pub expected_agent_pubkey: Option<Vec<u8>>,
}

/// Ferrite's frontend always prefixes browsed paths with `/` (its own
/// virtual-root convention, the same one `LocalBackend::resolve_path`
/// strips for the ferrite-server's own filesystem). A Ferrous agent has no
/// concept of that convention and just sees a real filesystem path on
/// whatever OS it runs on, so a Windows path arrives as `/D:/...` instead of
/// `D:/...`. Strip the leading slash only when what follows really looks
/// like a drive letter, so genuine Unix absolute paths (which also start
/// with `/`) are left untouched.
fn clean_ferrous_path(path: &str) -> String {
    let bytes = path.as_bytes();
    if path.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        path[1..].to_string()
    } else {
        path.to_string()
    }
}

/// One authenticated session with an agent. Requests on it are strictly
/// sequential (send, then read the one reply), which is all the file
/// operations need; the PTY path has its own duplex handling in
/// `ferrous_pty`.
struct Connection {
    noise: NoiseSession,
    stream: TcpStream,
}

impl Connection {
    async fn request(&mut self, req: &FerrousRequest) -> Result<FerrousResponse, RemoteError> {
        let req_json = serde_json::to_vec(req).map_err(|e| {
            RemoteError::OperationFailed(format!("Failed to serialize Ferrous request: {}", e))
        })?;

        self.noise.send_message(&mut self.stream, &req_json).await.map_err(|e| {
            RemoteError::OperationFailed(format!("Noise encrypt/send error: {}", e))
        })?;

        let plain = self.noise.read_message(&mut self.stream).await.map_err(|e| {
            RemoteError::OperationFailed(format!("Noise receive/decrypt error: {}", e))
        })?;

        let response: FerrousResponse = serde_json::from_slice(&plain).map_err(|e| {
            RemoteError::OperationFailed(format!("Failed to parse Ferrous response: {}", e))
        })?;

        match response {
            FerrousResponse::Error { message } => Err(RemoteError::OperationFailed(message)),
            FerrousResponse::Forbidden { message } => Err(RemoteError::Forbidden(message)),
            other => Ok(other),
        }
    }

    async fn read_chunk(&mut self, path: &str, offset: u64) -> Result<(Vec<u8>, bool), RemoteError> {
        let req = FerrousRequest::ReadChunk { path: path.to_string(), offset, len: TRANSFER_CHUNK as u32 };
        match self.request(&req).await? {
            FerrousResponse::Chunk { data, eof } => Ok((data, eof)),
            _ => Err(RemoteError::OperationFailed("Unexpected response type".into())),
        }
    }

    async fn write_chunk(&mut self, path: &str, offset: u64, data: Vec<u8>, truncate: bool) -> Result<(), RemoteError> {
        let req = FerrousRequest::WriteChunk { path: path.to_string(), offset, data, truncate };
        match self.request(&req).await? {
            FerrousResponse::Success => Ok(()),
            _ => Err(RemoteError::OperationFailed("Unexpected response type".into())),
        }
    }
}

impl FerrousBackend {
    pub fn new(
        host: String,
        port: u16,
        client_keypair: Arc<Keypair>,
        expected_agent_pubkey: Option<Vec<u8>>,
    ) -> Self {
        Self { host, port, client_keypair, expected_agent_pubkey }
    }

    /// Dials the agent, runs the mutual-auth handshake, checks the pin and
    /// exchanges versions. Shared with `FerrousPtySession`.
    pub(crate) async fn open_session(
        host: &str,
        port: u16,
        client_keypair: &Keypair,
        expected_agent_pubkey: Option<&[u8]>,
    ) -> Result<(NoiseSession, TcpStream), RemoteError> {
        let addr = format!("{}:{}", host, port);
        let connect = async {
            let mut stream = TcpStream::connect(&addr).await.map_err(|e| {
                RemoteError::ConnectionFailed(format!("Failed to connect to Ferrous agent at {}: {}", addr, e))
            })?;
            let (noise, remote_static) = NoiseSession::handshake_initiator(&mut stream, client_keypair)
                .await
                .map_err(|e| RemoteError::ConnectionFailed(format!(
                    "Noise handshake with Ferrous agent at {} failed (agent may not recognize this client's key): {}",
                    addr, e
                )))?;
            Ok::<_, RemoteError>((noise, stream, remote_static))
        };
        let (noise, stream, remote_static) = tokio::time::timeout(CONNECT_TIMEOUT, connect)
            .await
            .map_err(|_| RemoteError::ConnectionFailed(format!("Timed out connecting to Ferrous agent at {}", addr)))??;

        match expected_agent_pubkey {
            Some(expected) if remote_static != expected => {
                return Err(RemoteError::Unauthorized(format!(
                    "Ferrous agent at {} presented an unexpected identity key. It may have been reinstalled, \
                     or this could be an impersonation attempt. Update agent_pubkey in ferrite.yaml if the \
                     change is expected.",
                    addr
                )));
            }
            Some(_) => {}
            None => tracing::warn!(
                "No agent_pubkey pinned for Ferrous agent at {}; trusting its identity on this connection.",
                addr
            ),
        }

        // Version exchange: both sides learn the other's release before any
        // real request, so an upgrade of only one twin is reported as such.
        let mut conn = Connection { noise, stream };
        let hello = FerrousRequest::Hello {
            protocol_version: ferrum_core::PROTOCOL_VERSION,
            version: ferrum_core::VERSION.to_string(),
        };
        match conn.request(&hello).await {
            Ok(FerrousResponse::Hello { protocol_version, version }) if protocol_version == ferrum_core::PROTOCOL_VERSION => {
                tracing::debug!("Ferrous agent at {} is v{} (protocol {})", addr, version, protocol_version);
            }
            Ok(FerrousResponse::Hello { protocol_version, version }) => {
                return Err(RemoteError::ConnectionFailed(format!(
                    "Ferrous agent at {} is v{} (protocol {}); this Ferrite is v{} (protocol {}). \
                     Upgrade both twins to the same release.",
                    addr, version, protocol_version, ferrum_core::VERSION, ferrum_core::PROTOCOL_VERSION
                )));
            }
            Ok(_) => {
                return Err(RemoteError::ConnectionFailed(format!(
                    "Ferrous agent at {} did not answer the version exchange; it predates v0.2.0. \
                     Upgrade both twins to the same release.",
                    addr
                )));
            }
            Err(RemoteError::OperationFailed(message)) => {
                return Err(RemoteError::ConnectionFailed(format!("Ferrous agent at {} refused the session: {}", addr, message)));
            }
            Err(e) => return Err(e),
        }
        let Connection { noise, stream } = conn;
        Ok((noise, stream))
    }

    async fn connect(&self) -> Result<Connection, RemoteError> {
        let (noise, stream) = Self::open_session(
            &self.host,
            self.port,
            &self.client_keypair,
            self.expected_agent_pubkey.as_deref(),
        )
        .await?;
        Ok(Connection { noise, stream })
    }

    async fn send_request(&self, req: FerrousRequest) -> Result<FerrousResponse, RemoteError> {
        self.connect().await?.request(&req).await
    }

    fn expect_success(resp: FerrousResponse, what: &str) -> Result<(), RemoteError> {
        if matches!(resp, FerrousResponse::Success) {
            Ok(())
        } else {
            Err(RemoteError::OperationFailed(format!("Failed to {}", what)))
        }
    }
}

#[async_trait]
impl RemoteFilesystem for FerrousBackend {
    async fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>, RemoteError> {
        match self.send_request(FerrousRequest::ListDir { path: clean_ferrous_path(path) }).await? {
            FerrousResponse::ListDir { entries } => Ok(entries),
            _ => Err(RemoteError::OperationFailed("Unexpected response type".into())),
        }
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, RemoteError> {
        let path = clean_ferrous_path(path);
        let mut conn = self.connect().await?;
        let mut out = Vec::new();
        loop {
            let (data, eof) = conn.read_chunk(&path, out.len() as u64).await?;
            out.extend_from_slice(&data);
            if eof {
                return Ok(out);
            }
        }
    }

    async fn stream_file(&self, path: &str) -> Result<BoxStream<'static, Result<Bytes, RemoteError>>, RemoteError> {
        let path = clean_ferrous_path(path);
        let mut conn = self.connect().await?;
        // First chunk up front, so a missing file or a denied path surfaces
        // as an error from this call rather than as a broken download.
        let (first, mut eof) = conn.read_chunk(&path, 0).await?;
        let mut offset = first.len() as u64;

        let (tx, rx) = mpsc::channel::<Result<Bytes, RemoteError>>(4);
        tokio::spawn(async move {
            if tx.send(Ok(Bytes::from(first))).await.is_err() {
                return;
            }
            while !eof {
                match conn.read_chunk(&path, offset).await {
                    Ok((data, done)) => {
                        offset += data.len() as u64;
                        eof = done;
                        if !data.is_empty() && tx.send(Ok(Bytes::from(data))).await.is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                }
            }
        });

        Ok(Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })))
    }

    async fn write_file(&self, path: &str, content: &[u8]) -> Result<(), RemoteError> {
        let path = clean_ferrous_path(path);
        let mut conn = self.connect().await?;
        if content.is_empty() {
            return conn.write_chunk(&path, 0, Vec::new(), true).await;
        }
        let mut offset = 0u64;
        for chunk in content.chunks(TRANSFER_CHUNK) {
            conn.write_chunk(&path, offset, chunk.to_vec(), offset == 0).await?;
            offset += chunk.len() as u64;
        }
        Ok(())
    }

    async fn write_stream(
        &self,
        path: &str,
        mut stream: BoxStream<'static, Result<Bytes, RemoteError>>,
    ) -> Result<(), RemoteError> {
        let path = clean_ferrous_path(path);
        let mut conn = self.connect().await?;
        let mut offset = 0u64;
        // Coalesce small incoming pieces into full wire chunks: one request
        // per kilobyte would make a large upload crawl.
        let mut pending: Vec<u8> = Vec::with_capacity(TRANSFER_CHUNK);
        let mut first = true;
        while let Some(piece) = stream.next().await {
            pending.extend_from_slice(&piece?);
            while pending.len() >= TRANSFER_CHUNK {
                let rest = pending.split_off(TRANSFER_CHUNK);
                let chunk = std::mem::replace(&mut pending, rest);
                conn.write_chunk(&path, offset, chunk, first).await?;
                offset += TRANSFER_CHUNK as u64;
                first = false;
            }
        }
        if first || !pending.is_empty() {
            conn.write_chunk(&path, offset, pending, first).await?;
        }
        Ok(())
    }

    async fn stat(&self, path: &str) -> Result<FileMeta, RemoteError> {
        match self.send_request(FerrousRequest::Stat { path: clean_ferrous_path(path) }).await? {
            FerrousResponse::Stat { meta } => Ok(meta),
            _ => Err(RemoteError::OperationFailed("Unexpected response type".into())),
        }
    }

    async fn delete(&self, path: &str) -> Result<(), RemoteError> {
        let resp = self.send_request(FerrousRequest::Delete { path: clean_ferrous_path(path) }).await?;
        Self::expect_success(resp, "delete entry")
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), RemoteError> {
        let resp = self
            .send_request(FerrousRequest::Rename { from: clean_ferrous_path(from), to: clean_ferrous_path(to) })
            .await?;
        Self::expect_success(resp, "rename entry")
    }

    async fn create_dir(&self, path: &str) -> Result<(), RemoteError> {
        let resp = self.send_request(FerrousRequest::CreateDir { path: clean_ferrous_path(path) }).await?;
        Self::expect_success(resp, "create directory")
    }

    async fn create_file(&self, path: &str) -> Result<(), RemoteError> {
        let resp = self.send_request(FerrousRequest::CreateFile { path: clean_ferrous_path(path) }).await?;
        Self::expect_success(resp, "create file")
    }

    async fn search(
        &self,
        path: &str,
        query: &str,
        content_search: bool,
    ) -> Result<Vec<SearchMatch>, RemoteError> {
        let req = FerrousRequest::Search { path: clean_ferrous_path(path), query: query.to_string(), content_search };
        match self.send_request(req).await? {
            FerrousResponse::Search { matches } => Ok(matches),
            _ => Err(RemoteError::OperationFailed("Unexpected response type".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_the_virtual_root_only_before_a_drive_letter() {
        assert_eq!(clean_ferrous_path("/C:/Users"), "C:/Users");
        assert_eq!(clean_ferrous_path("/srv/data"), "/srv/data");
        assert_eq!(clean_ferrous_path("/"), "/");
    }
}
