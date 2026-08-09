mod pty;
mod server;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use clap::Parser;
use serde::Deserialize;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpListener;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use ferrum_core::{Capability, FerrousRequest, FerrousResponse, NoiseSession};
use server::FerrousServer;

type ClientMap = HashMap<Vec<u8>, Capability>;

#[derive(Parser)]
#[command(name = "ferrous-agent")]
#[command(version = ferrum_core::FULL_VERSION_INFO)]
#[command(about = "Ferrous server-side agent daemon")]
struct Cli {
    #[arg(short, long, default_value = "127.0.0.1:9090")]
    bind: String,

    #[arg(long, default_value = "ferrous_identity.key")]
    identity_path: PathBuf,

    #[arg(long, help = "Load bind/identity/authorized clients from a ferrous.yaml file instead of CLI flags")]
    config: Option<PathBuf>,

    #[arg(long, default_value = "false")]
    read_only: bool,

    #[arg(long, default_value = "false")]
    no_shell: bool,

    #[arg(long)]
    allowed_path: Vec<String>,

    #[arg(long, help = "Hex-encoded client public key to authorize (repeatable). Required unless --config is used.")]
    authorize_client: Vec<String>,

    #[arg(long, help = "Drop root privileges to this user after binding (Unix only)")]
    run_as_user: Option<String>,

    #[arg(long, help = "Group to drop to alongside --run-as-user (defaults to the user's primary group)")]
    run_as_group: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FileConfig {
    #[serde(default = "default_bind")]
    bind: String,
    #[serde(default = "default_identity_path")]
    identity_path: String,
    #[serde(default)]
    clients: Vec<ClientEntry>,
    #[serde(default)]
    run_as_user: Option<String>,
    #[serde(default)]
    run_as_group: Option<String>,
}

fn default_bind() -> String {
    "127.0.0.1:9090".to_string()
}

fn default_identity_path() -> String {
    "ferrous_identity.key".to_string()
}

#[derive(Debug, Deserialize)]
struct ClientEntry {
    pubkey: String,
    #[serde(default)]
    allowed_paths: Vec<String>,
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    allow_shell: bool,
}

fn client_capability(allowed_paths: Vec<String>, read_only: bool, allow_shell: bool) -> Capability {
    Capability {
        allowed_paths: if allowed_paths.is_empty() { vec!["/".to_string()] } else { allowed_paths },
        read_only,
        allow_shell,
    }
}

struct Settings {
    bind: String,
    identity_path: PathBuf,
    clients: ClientMap,
    run_as_user: Option<String>,
    run_as_group: Option<String>,
}

fn load_settings(cli: &Cli) -> Result<Settings, Box<dyn std::error::Error>> {
    if let Some(config_path) = &cli.config {
        let content = std::fs::read_to_string(config_path)?;
        let file_config: FileConfig = serde_yaml::from_str(&content)?;

        let mut clients = ClientMap::new();
        for entry in file_config.clients {
            let pubkey = ferrum_core::from_hex(&entry.pubkey)
                .ok_or_else(|| format!("Invalid hex pubkey in config: {}", entry.pubkey))?;
            clients.insert(pubkey, client_capability(entry.allowed_paths, entry.read_only, entry.allow_shell));
        }

        Ok(Settings {
            bind: file_config.bind,
            identity_path: PathBuf::from(file_config.identity_path),
            clients,
            run_as_user: file_config.run_as_user,
            run_as_group: file_config.run_as_group,
        })
    } else {
        let capability = client_capability(cli.allowed_path.clone(), cli.read_only, !cli.no_shell);
        let mut clients = ClientMap::new();
        for hex_key in &cli.authorize_client {
            let pubkey = ferrum_core::from_hex(hex_key)
                .ok_or_else(|| format!("Invalid hex pubkey: {}", hex_key))?;
            clients.insert(pubkey, capability.clone());
        }
        Ok(Settings {
            bind: cli.bind.clone(),
            identity_path: cli.identity_path.clone(),
            clients,
            run_as_user: cli.run_as_user.clone(),
            run_as_group: cli.run_as_group.clone(),
        })
    }
}

#[cfg(unix)]
fn drop_privileges(user: &str, group: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    use nix::unistd::{setgid, setuid, Group, Uid, User};

    if !Uid::effective().is_root() {
        tracing::warn!("--run-as-user was given but the process is not running as root; skipping privilege drop.");
        return Ok(());
    }

    let user_info = User::from_name(user)?.ok_or_else(|| format!("User '{}' not found", user))?;
    let gid = if let Some(group_name) = group {
        Group::from_name(group_name)?
            .ok_or_else(|| format!("Group '{}' not found", group_name))?
            .gid
    } else {
        user_info.gid
    };

    // Group before user: dropping the uid first would strip the permission
    // needed to still change the gid.
    setgid(gid)?;
    setuid(user_info.uid)?;

    tracing::info!("Dropped privileges to user '{}' (uid={}, gid={})", user, user_info.uid, gid);
    Ok(())
}

#[cfg(not(unix))]
fn drop_privileges(_user: &str, _group: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    tracing::warn!("--run-as-user is only supported on Unix; ignoring on this platform.");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();
    tracing::info!("Starting Ferrous Agent {}", ferrum_core::FULL_VERSION_INFO);
    let settings = load_settings(&cli)?;

    let identity = ferrum_core::load_or_generate_keypair(&settings.identity_path)?;
    tracing::info!(
        "Ferrous agent identity public key (share with clients you authorize): {}",
        ferrum_core::to_hex(&identity.public)
    );

    if settings.clients.is_empty() {
        tracing::error!(
            "No authorized clients configured. Pass --authorize-client <hex-pubkey> (repeatable) or use --config with a clients: list."
        );
        return Err("no authorized clients configured".into());
    }
    tracing::info!("{} authorized client key(s) loaded", settings.clients.len());

    let addr: SocketAddr = settings.bind.parse()?;
    let identity = Arc::new(identity);
    let clients = Arc::new(settings.clients);
    let server = Arc::new(FerrousServer::new());
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("Ferrous agent listening on Noise_XX transport at {}", addr);

    // Bind first (may need root for a low port), then permanently drop root
    // before accepting any connections or spawning shells.
    if let Some(user) = &settings.run_as_user {
        drop_privileges(user, settings.run_as_group.as_deref())?;
    }

    loop {
        let (mut stream, peer_addr) = listener.accept().await?;
        tracing::info!("Incoming connection from {}", peer_addr);
        let server = Arc::clone(&server);
        let clients = Arc::clone(&clients);
        let identity = Arc::clone(&identity);

        tokio::spawn(async move {
            let (noise, remote_static) = match NoiseSession::handshake_responder(&mut stream, &identity).await {
                Ok(session) => session,
                Err(e) => {
                    tracing::error!("Noise handshake failed for {}: {}", peer_addr, e);
                    return;
                }
            };

            let capability = match clients.get(&remote_static) {
                Some(cap) => cap.clone(),
                None => {
                    tracing::warn!(
                        "Rejected connection from {}: unrecognized client key {}",
                        peer_addr,
                        ferrum_core::to_hex(&remote_static)
                    );
                    return;
                }
            };
            tracing::info!(
                "Authenticated session with {} (client {})",
                peer_addr,
                ferrum_core::to_hex(&remote_static)
            );

            loop {
                let msg_bytes = match noise.read_message(&mut stream).await {
                    Ok(bytes) => bytes,
                    Err(_) => break,
                };

                let req: FerrousRequest = match serde_json::from_slice(&msg_bytes) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!("Failed to parse request from {}: {}", peer_addr, e);
                        break;
                    }
                };

                // PTY hands the connection off to a dedicated duplex session for the
                // rest of its lifetime instead of the one-shot request/response loop.
                if let FerrousRequest::PtyOpen { cols, rows } = req {
                    if !capability.allow_shell {
                        let resp = FerrousResponse::Error {
                            message: "Shell access disabled by capability policy".into(),
                        };
                        if let Ok(bytes) = serde_json::to_vec(&resp) {
                            let _ = noise.send_message(&mut stream, &bytes).await;
                        }
                        break;
                    }
                    let (read_half, write_half) = stream.into_split();
                    run_pty_session(read_half, write_half, noise, capability, cols, rows, peer_addr).await;
                    return;
                }

                let response = server.handle_request(req, &capability).await;
                let resp_bytes = match serde_json::to_vec(&response) {
                    Ok(b) => b,
                    Err(_) => break,
                };

                if noise.send_message(&mut stream, &resp_bytes).await.is_err() {
                    break;
                }
            }
            tracing::info!("Session ended with {}", peer_addr);
        });
    }
}

async fn run_pty_session(
    mut read_half: OwnedReadHalf,
    mut write_half: OwnedWriteHalf,
    noise: NoiseSession,
    capability: Capability,
    cols: u32,
    rows: u32,
    peer_addr: SocketAddr,
) {
    let (pty_handle, tx_in, mut rx_out) = match pty::spawn(&capability, cols, rows) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("Failed to spawn PTY for {}: {}", peer_addr, e);
            let resp = FerrousResponse::Error { message: e };
            if let Ok(bytes) = serde_json::to_vec(&resp) {
                let _ = noise.send_message(&mut write_half, &bytes).await;
            }
            return;
        }
    };

    let ack = serde_json::to_vec(&FerrousResponse::Success).unwrap();
    if noise.send_message(&mut write_half, &ack).await.is_err() {
        return;
    }
    tracing::info!("PTY session started for {}", peer_addr);

    let reader_noise = noise.clone();
    let resize_handle = pty_handle.clone();
    let mut request_task = tokio::spawn(async move {
        loop {
            let bytes = match reader_noise.read_message(&mut read_half).await {
                Ok(b) => b,
                Err(_) => break,
            };
            match serde_json::from_slice::<FerrousRequest>(&bytes) {
                Ok(FerrousRequest::PtyInput { data }) => {
                    if tx_in.send(data).await.is_err() {
                        break;
                    }
                }
                Ok(FerrousRequest::PtyResize { cols, rows }) => {
                    resize_handle.resize(cols, rows);
                }
                _ => {}
            }
        }
    });

    let writer_noise = noise.clone();
    let mut response_task = tokio::spawn(async move {
        while let Some(data) = rx_out.recv().await {
            let resp = FerrousResponse::PtyOutput { data };
            let Ok(bytes) = serde_json::to_vec(&resp) else { continue };
            if writer_noise.send_message(&mut write_half, &bytes).await.is_err() {
                break;
            }
        }
    });

    tokio::select! {
        _ = &mut request_task => response_task.abort(),
        _ = &mut response_task => request_task.abort(),
    }
    tracing::info!("PTY session ended for {}", peer_addr);
}
