use async_trait::async_trait;
use ferrum_core::{
    FileEntry, FileMeta, RemoteError, RemoteFilesystem, SearchMatch,
    FerrousRequest, FerrousResponse, NoiseSession,
};
use snow::Keypair;
use std::sync::Arc;
use tokio::net::TcpStream;

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

impl FerrousBackend {
    pub fn new(
        host: String,
        port: u16,
        client_keypair: Arc<Keypair>,
        expected_agent_pubkey: Option<Vec<u8>>,
    ) -> Self {
        Self { host, port, client_keypair, expected_agent_pubkey }
    }

    async fn send_request(&self, req: FerrousRequest) -> Result<FerrousResponse, RemoteError> {
        let addr = format!("{}:{}", self.host, self.port);
        let mut stream = TcpStream::connect(&addr).await.map_err(|e| {
            RemoteError::ConnectionFailed(format!("Failed to connect to Ferrous agent at {}: {}", addr, e))
        })?;

        let (noise, remote_static) = NoiseSession::handshake_initiator(&mut stream, &self.client_keypair)
            .await
            .map_err(|e| RemoteError::ConnectionFailed(format!(
                "Noise handshake with Ferrous agent at {} failed (agent may not recognize this client's key): {}",
                addr, e
            )))?;

        if let Some(expected) = &self.expected_agent_pubkey {
            if &remote_static != expected {
                return Err(RemoteError::Unauthorized(format!(
                    "Ferrous agent at {} presented an unexpected identity key. It may have been reinstalled, \
                     or this could be an impersonation attempt. Update agent_pubkey in ferrite.yaml if the \
                     change is expected.",
                    addr
                )));
            }
        } else {
            tracing::warn!(
                "No agent_pubkey pinned for Ferrous agent at {}; trusting its identity on this connection.",
                addr
            );
        }

        let req_json = serde_json::to_vec(&req).map_err(|e| {
            RemoteError::OperationFailed(format!("Failed to serialize Ferrous request: {}", e))
        })?;

        noise.send_message(&mut stream, &req_json).await.map_err(|e| {
            RemoteError::OperationFailed(format!("Noise encrypt/send error: {}", e))
        })?;

        let plain = noise.read_message(&mut stream).await.map_err(|e| {
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
}

#[async_trait]
impl RemoteFilesystem for FerrousBackend {
    async fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>, RemoteError> {
        let resp = self.send_request(FerrousRequest::ListDir { path: clean_ferrous_path(path) }).await?;
        if let FerrousResponse::ListDir { entries } = resp {
            Ok(entries)
        } else {
            Err(RemoteError::OperationFailed("Unexpected response type".into()))
        }
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, RemoteError> {
        let resp = self.send_request(FerrousRequest::ReadFile { path: clean_ferrous_path(path) }).await?;
        if let FerrousResponse::ReadFile { data } = resp {
            Ok(data)
        } else {
            Err(RemoteError::OperationFailed("Unexpected response type".into()))
        }
    }

    async fn write_file(&self, path: &str, content: &[u8]) -> Result<(), RemoteError> {
        let resp = self.send_request(FerrousRequest::WriteFile {
            path: clean_ferrous_path(path),
            content: content.to_vec(),
        }).await?;
        if matches!(resp, FerrousResponse::Success) {
            Ok(())
        } else {
            Err(RemoteError::OperationFailed("Failed to write file".into()))
        }
    }

    async fn stat(&self, path: &str) -> Result<FileMeta, RemoteError> {
        let resp = self.send_request(FerrousRequest::Stat { path: clean_ferrous_path(path) }).await?;
        if let FerrousResponse::Stat { meta } = resp {
            Ok(meta)
        } else {
            Err(RemoteError::OperationFailed("Unexpected response type".into()))
        }
    }

    async fn delete(&self, path: &str) -> Result<(), RemoteError> {
        let resp = self.send_request(FerrousRequest::Delete { path: clean_ferrous_path(path) }).await?;
        if matches!(resp, FerrousResponse::Success) {
            Ok(())
        } else {
            Err(RemoteError::OperationFailed("Failed to delete entry".into()))
        }
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), RemoteError> {
        let resp = self.send_request(FerrousRequest::Rename {
            from: clean_ferrous_path(from),
            to: clean_ferrous_path(to),
        }).await?;
        if matches!(resp, FerrousResponse::Success) {
            Ok(())
        } else {
            Err(RemoteError::OperationFailed("Failed to rename entry".into()))
        }
    }

    async fn create_dir(&self, path: &str) -> Result<(), RemoteError> {
        let resp = self.send_request(FerrousRequest::CreateDir { path: clean_ferrous_path(path) }).await?;
        if matches!(resp, FerrousResponse::Success) {
            Ok(())
        } else {
            Err(RemoteError::OperationFailed("Failed to create directory".into()))
        }
    }

    async fn create_file(&self, path: &str) -> Result<(), RemoteError> {
        let resp = self.send_request(FerrousRequest::CreateFile { path: clean_ferrous_path(path) }).await?;
        if matches!(resp, FerrousResponse::Success) {
            Ok(())
        } else {
            Err(RemoteError::OperationFailed("Failed to create file".into()))
        }
    }

    async fn search(
        &self,
        path: &str,
        query: &str,
        content_search: bool,
    ) -> Result<Vec<SearchMatch>, RemoteError> {
        let resp = self.send_request(FerrousRequest::Search {
            path: clean_ferrous_path(path),
            query: query.to_string(),
            content_search,
        }).await?;
        if let FerrousResponse::Search { matches } = resp {
            Ok(matches)
        } else {
            Err(RemoteError::OperationFailed("Unexpected response type".into()))
        }
    }

}
