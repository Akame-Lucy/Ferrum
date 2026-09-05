use ferrum_core::{FerrousRequest, FerrousResponse, RemoteError};
use snow::Keypair;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::ferrous_client::FerrousBackend;

/// A live shell session on a Ferrous agent, matching `SshPtySession`'s public
/// shape so callers can treat the two interchangeably.
pub struct FerrousPtySession {
    resize_tx: mpsc::UnboundedSender<(u32, u32)>,
}

impl FerrousPtySession {
    pub async fn connect(
        host: String,
        port: u16,
        client_keypair: Arc<Keypair>,
        expected_agent_pubkey: Option<Vec<u8>>,
        cols: u32,
        rows: u32,
    ) -> Result<(Self, mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>), RemoteError> {
        let (noise, stream) =
            FerrousBackend::open_session(&host, port, &client_keypair, expected_agent_pubkey.as_deref()).await?;
        let (mut read_half, mut write_half) = stream.into_split();

        let open_req = serde_json::to_vec(&FerrousRequest::PtyOpen { cols, rows })
            .map_err(|e| RemoteError::OperationFailed(format!("Failed to serialize PtyOpen: {}", e)))?;
        noise.send_message(&mut write_half, &open_req).await.map_err(|e| {
            RemoteError::OperationFailed(format!("Failed to open PTY on Ferrous agent: {}", e))
        })?;
        let resp_bytes = noise.read_message(&mut read_half).await.map_err(|e| {
            RemoteError::OperationFailed(format!("Failed to open PTY on Ferrous agent: {}", e))
        })?;
        match serde_json::from_slice::<FerrousResponse>(&resp_bytes) {
            Ok(FerrousResponse::Success) => {}
            Ok(FerrousResponse::Error { message }) => return Err(RemoteError::OperationFailed(message)),
            Ok(FerrousResponse::Forbidden { message }) => return Err(RemoteError::Forbidden(message)),
            _ => return Err(RemoteError::OperationFailed("Unexpected response opening PTY".into())),
        }

        let (tx_in, mut rx_in) = mpsc::channel::<Vec<u8>>(128);
        let (tx_out, rx_out) = mpsc::channel::<Vec<u8>>(128);
        let (resize_tx, mut resize_rx) = mpsc::unbounded_channel::<(u32, u32)>();

        let writer_noise = noise.clone();
        tokio::spawn(async move {
            loop {
                let req = tokio::select! {
                    data = rx_in.recv() => match data {
                        Some(data) => FerrousRequest::PtyInput { data },
                        None => break,
                    },
                    resize = resize_rx.recv() => match resize {
                        Some((cols, rows)) => FerrousRequest::PtyResize { cols, rows },
                        None => break,
                    },
                };
                let Ok(bytes) = serde_json::to_vec(&req) else { continue };
                if writer_noise.send_message(&mut write_half, &bytes).await.is_err() {
                    break;
                }
            }
        });

        let reader_noise = noise;
        tokio::spawn(async move {
            loop {
                let bytes = match reader_noise.read_message(&mut read_half).await {
                    Ok(b) => b,
                    Err(_) => break,
                };
                match serde_json::from_slice::<FerrousResponse>(&bytes) {
                    Ok(FerrousResponse::PtyOutput { data }) => {
                        if tx_out.send(data).await.is_err() {
                            break;
                        }
                    }
                    Ok(FerrousResponse::Error { message }) => {
                        tracing::warn!("Ferrous PTY error: {}", message);
                        break;
                    }
                    _ => {}
                }
            }
        });

        Ok((Self { resize_tx }, tx_in, rx_out))
    }

    pub fn resize(&self, cols: u32, rows: u32) -> Result<(), RemoteError> {
        let _ = self.resize_tx.send((cols, rows));
        Ok(())
    }
}
