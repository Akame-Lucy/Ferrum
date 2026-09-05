//! WebSocket endpoints: the terminal, live-edit collaboration rooms, and the
//! plugin event feed. Cross-origin upgrades are refused before any of these
//! run (see `same_origin_guard` in `main.rs`).

use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::broadcast;
use tower_cookies::Cookies;

use super::{resolve_agent_pin, ApiError, AppState, PluginEvent};
use crate::permissions::authorize;
use ferrite_core::{ProtocolType, RemoteError};
use ferrite_pty::SshPtySession;

/// A collaboration message is an editor operation, not a file.
const MAX_COLLAB_MESSAGE: usize = 256 * 1024;

#[derive(Debug, Deserialize)]
pub struct TerminalQuery {
    pub remote: Option<String>,
    pub cols: Option<u32>,
    pub rows: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct CollabQuery {
    pub remote: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum TerminalControlMessage {
    #[serde(rename = "resize")]
    Resize { cols: u32, rows: u32 },
}

pub async fn collab_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<CollabQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    // Collaboration edits the file, so it takes the write grant.
    authorize(&user, &query.remote, &query.path, true)?;
    let room_id = format!("{}:{}", query.remote, query.path);
    let room_tx = state.get_or_create_collab_room(&room_id);
    Ok(ws.max_message_size(MAX_COLLAB_MESSAGE).on_upgrade(move |socket| handle_collab_socket(socket, room_tx)))
}

async fn handle_collab_socket(socket: WebSocket, room_tx: broadcast::Sender<String>) {
    let (mut sender, mut receiver) = socket.split();
    let mut rx = room_tx.subscribe();

    let mut send_task = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            if sender.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
    });

    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            if let Message::Text(text) = msg {
                let _ = room_tx.send(text);
            }
        }
    });

    tokio::select! {
        _ = &mut send_task => recv_task.abort(),
        _ = &mut recv_task => send_task.abort(),
    };
}

pub async fn events_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
) -> Result<impl IntoResponse, ApiError> {
    state.current_user(&cookies)?;
    let event_tx = state.event_tx.clone();
    Ok(ws.on_upgrade(move |socket| handle_events_socket(socket, event_tx)))
}

async fn handle_events_socket(socket: WebSocket, event_tx: broadcast::Sender<PluginEvent>) {
    let (mut sender, _) = socket.split();
    let mut rx = event_tx.subscribe();

    while let Ok(event) = rx.recv().await {
        if let Ok(json) = serde_json::to_string(&event) {
            if sender.send(Message::Text(json)).await.is_err() {
                break;
            }
        }
    }
}

pub async fn terminal_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<TerminalQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    if !user.allow_shell {
        return Err(ApiError(RemoteError::Forbidden("Shell access is disabled for this account".into())));
    }
    let remote_config = {
        let config = state.config.lock().unwrap();
        if let Some(terminal) = config.terminal.clone() {
            Some(terminal)
        } else if let Some(name) = query.remote.as_ref() {
            // A shell on a remote is at least as powerful as a write grant on
            // it, so a user has to be allowed the remote at all before they
            // can pick it as a terminal target.
            if !user.can_use_remote(name) {
                return Err(ApiError(RemoteError::Forbidden(format!("No access to remote '{}'", name))));
            }
            config.get_remote(name).cloned()
        } else {
            None
        }
    }
    .ok_or_else(|| RemoteError::OperationFailed(
        "No terminal SSH host configured. Add a 'terminal:' section to ferrite.yaml.".into(),
    ))?;

    let cols = query.cols.unwrap_or(80).clamp(1, 1000);
    let rows = query.rows.unwrap_or(24).clamp(1, 500);
    tracing::info!("Terminal session opened by '{}' to {}:{}", user.username, remote_config.host, remote_config.port);

    Ok(ws.on_upgrade(move |socket| handle_terminal_socket(socket, state, remote_config, cols, rows)))
}

/// The upgrade already succeeded by this point, so the only way to tell the user
/// why the terminal is empty is to write the reason into the xterm stream.
async fn report_terminal_failure(mut socket: WebSocket, reason: &str) {
    let _ = socket
        .send(Message::Text(format!("\r\n\x1b[31m{}\x1b[0m\r\n", reason)))
        .await;
    let _ = socket.send(Message::Close(None)).await;
}

enum PtyBackend {
    Ssh(SshPtySession),
    Ferrous(ferrite_core::FerrousPtySession),
}

impl PtyBackend {
    fn resize(&self, cols: u32, rows: u32) -> Result<(), RemoteError> {
        match self {
            PtyBackend::Ssh(s) => s.resize(cols, rows),
            PtyBackend::Ferrous(s) => s.resize(cols, rows),
        }
    }
}

async fn handle_terminal_socket(
    socket: WebSocket,
    state: Arc<AppState>,
    remote_config: ferrite_core::RemoteConfig,
    cols: u32,
    rows: u32,
) {
    let host = format!("{}:{}", remote_config.host, remote_config.port);
    let remote_config = remote_config.with_secrets_resolved();

    let (pty_session, tx_pty_in, mut rx_pty_out) = if remote_config.protocol == ProtocolType::Ferrous {
        let client_keypair = state.client_keypair.clone();
        // A malformed pin must not open an unpinned shell. Fail the session and
        // say why, rather than connecting to an agent nobody verified.
        let expected_agent_pubkey = match resolve_agent_pin(&remote_config) {
            Ok(pin) => pin,
            Err(e) => {
                tracing::warn!("Terminal Ferrous connect to {} refused: {}", host, e);
                report_terminal_failure(socket, &format!("{}", e)).await;
                return;
            }
        };
        match ferrite_core::FerrousPtySession::connect(
            remote_config.host.clone(),
            remote_config.port,
            client_keypair,
            expected_agent_pubkey,
            cols,
            rows,
        ).await {
            Ok((session, tx, rx)) => (PtyBackend::Ferrous(session), tx, rx),
            Err(e) => {
                tracing::warn!("Terminal Ferrous connect to {} failed: {}", host, e);
                report_terminal_failure(socket, &format!("Ferrous connection to {} failed: {}", host, e)).await;
                return;
            }
        }
    } else {
        match tokio::task::spawn_blocking(move || SshPtySession::connect(remote_config, cols, rows)).await {
            Ok(Ok((session, tx, rx))) => (PtyBackend::Ssh(session), tx, rx),
            Ok(Err(e)) => {
                tracing::warn!("Terminal SSH connect to {} failed: {}", host, e);
                report_terminal_failure(socket, &format!("SSH connection to {} failed: {}", host, e)).await;
                return;
            }
            Err(e) => {
                tracing::error!("Terminal connect task panicked: {}", e);
                report_terminal_failure(socket, "Internal error while opening the SSH session.").await;
                return;
            }
        }
    };

    let pty_session = Arc::new(pty_session);
    let (mut ws_sender, mut ws_receiver) = socket.split();

    let pty_session_clone = Arc::clone(&pty_session);
    let mut receive_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_receiver.next().await {
            match msg {
                Message::Text(text) => {
                    if let Ok(TerminalControlMessage::Resize { cols, rows }) =
                        serde_json::from_str::<TerminalControlMessage>(&text)
                    {
                        let session = Arc::clone(&pty_session_clone);
                        let _ = tokio::task::spawn_blocking(move || {
                            session.resize(cols.clamp(1, 1000), rows.clamp(1, 500))
                        })
                        .await;
                    } else if tx_pty_in.send(text.into_bytes()).await.is_err() {
                        break;
                    }
                }
                Message::Binary(bytes) => {
                    if tx_pty_in.send(bytes).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    let mut send_task = tokio::spawn(async move {
        while let Some(bytes) = rx_pty_out.recv().await {
            if ws_sender.send(Message::Binary(bytes)).await.is_err() {
                break;
            }
        }
    });

    tokio::select! {
        _ = &mut receive_task => send_task.abort(),
        _ = &mut send_task => receive_task.abort(),
    };
}
