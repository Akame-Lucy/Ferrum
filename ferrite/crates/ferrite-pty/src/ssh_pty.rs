use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use ssh2::{ExtendedData, Session};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::task;

use ferrite_core::{ProtocolType, RemoteConfig, RemoteError};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HANDSHAKE_TIMEOUT_MS: u32 = 15_000;

type PendingResize = Arc<Mutex<Option<(u32, u32)>>>;

/// A connected session plus the channels carrying bytes to and from the remote
/// shell: `Sender` feeds stdin, `Receiver` yields stdout/stderr.
type SshPtyChannels = (
    SshPtySession,
    mpsc::Sender<Vec<u8>>,
    mpsc::Receiver<Vec<u8>>,
);

pub struct SshPtySession {
    pending_resize: PendingResize,
}

impl SshPtySession {
    pub fn connect(config: RemoteConfig, cols: u32, rows: u32) -> Result<SshPtyChannels, RemoteError> {
        Self::connect_ssh(config, cols, rows)
    }

    fn ssh_port(config: &RemoteConfig) -> u16 {
        match config.protocol {
            ProtocolType::Sftp if config.port != 0 => config.port,
            _ => 22,
        }
    }

    fn connect_ssh(config: RemoteConfig, cols: u32, rows: u32) -> Result<SshPtyChannels, RemoteError> {
        let port = Self::ssh_port(&config);
        let addr = format!("{}:{}", config.host, port);
        let sock_addr = (config.host.as_str(), port)
            .to_socket_addrs()
            .map_err(|e| RemoteError::ConnectionFailed(format!("Failed to resolve {}: {}", addr, e)))?
            .next()
            .ok_or_else(|| RemoteError::ConnectionFailed(format!("No address found for {}", addr)))?;

        // Without an explicit timeout a dropped SYN leaves this blocking for the
        // OS default (~21s on Windows) with no feedback to the browser.
        let tcp = TcpStream::connect_timeout(&sock_addr, CONNECT_TIMEOUT)
            .map_err(|e| RemoteError::ConnectionFailed(format!("Failed to connect to {}: {}", addr, e)))?;

        let mut sess = Session::new()
            .map_err(|e| RemoteError::Ssh(format!("Failed to create SSH session: {}", e)))?;
        sess.set_timeout(HANDSHAKE_TIMEOUT_MS);
        sess.set_tcp_stream(tcp);
        sess.handshake()
            .map_err(|e| RemoteError::Ssh(format!("SSH handshake failed: {}", e)))?;

        if let Some(ref key_path) = config.auth.private_key {
            let expanded_path = shellexpand_path(key_path);
            let path = Path::new(&expanded_path);
            sess.userauth_pubkey_file(
                &config.username,
                None,
                path,
                config.auth.passphrase.as_deref(),
            )
            .map_err(|e| RemoteError::AuthFailed(format!("Key auth failed for {}: {}", config.username, e)))?;
        } else if let Some(ref password) = config.auth.password {
            sess.userauth_password(&config.username, password)
                .map_err(|e| RemoteError::AuthFailed(format!("Password auth failed for {}: {}", config.username, e)))?;
        } else {
            return Err(RemoteError::AuthFailed("No auth method provided in config".into()));
        }

        if !sess.authenticated() {
            return Err(RemoteError::AuthFailed(format!("Authentication failed for {}", config.username)));
        }

        let mut channel = sess.channel_session()
            .map_err(|e| RemoteError::Ssh(format!("Failed to open channel: {}", e)))?;

        channel.handle_extended_data(ExtendedData::Merge)
            .map_err(|e| RemoteError::Ssh(format!("Failed to set extended data: {}", e)))?;

        channel.request_pty("xterm-256color", None, Some((cols, rows, 0, 0)))
            .map_err(|e| RemoteError::Ssh(format!("Failed to request PTY: {}", e)))?;

        channel.shell()
            .map_err(|e| RemoteError::Ssh(format!("Failed to request shell: {}", e)))?;

        // The pump below polls, so drop the blocking-call timeout before
        // switching modes to avoid spurious timeout errors on idle reads.
        sess.set_timeout(0);
        sess.set_blocking(false);

        let pending_resize: PendingResize = Arc::new(Mutex::new(None));
        let (tx_in, mut rx_in) = mpsc::channel::<Vec<u8>>(128);
        let (tx_out, rx_out) = mpsc::channel::<Vec<u8>>(128);

        let resize_handle = Arc::clone(&pending_resize);
        task::spawn_blocking(move || {
            let _sess = sess;
            let mut write_buf: Vec<u8> = Vec::new();
            let mut read_buf = [0u8; 4096];

            loop {
                if let Ok(mut pending) = resize_handle.lock() {
                    if let Some((c, r)) = pending.take() {
                        let _ = channel.request_pty_size(c, r, None, None);
                    }
                }

                loop {
                    match rx_in.try_recv() {
                        Ok(data) => write_buf.extend_from_slice(&data),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return,
                    }
                }

                if !write_buf.is_empty() {
                    match channel.write(&write_buf) {
                        Ok(0) => {}
                        Ok(n) => {
                            write_buf.drain(..n);
                            let _ = channel.flush();
                        }
                        Err(ref e) if e.kind() == ErrorKind::WouldBlock => {}
                        Err(_) => return,
                    }
                }

                match channel.read(&mut read_buf) {
                    Ok(0) => {
                        if channel.eof() {
                            return;
                        }
                    }
                    Ok(n) => {
                        if tx_out.blocking_send(read_buf[..n].to_vec()).is_err() {
                            return;
                        }
                        continue;
                    }
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(_) => return,
                }

                std::thread::sleep(Duration::from_millis(8));
            }
        });

        let session_obj = Self { pending_resize };

        Ok((session_obj, tx_in, rx_out))
    }

    pub fn resize(&self, cols: u32, rows: u32) -> Result<(), RemoteError> {
        let mut pending = self.pending_resize.lock()
            .map_err(|_| RemoteError::OperationFailed("Lock poisoned".into()))?;
        *pending = Some((cols, rows));
        Ok(())
    }
}

fn shellexpand_path(p: &str) -> String {
    if p.starts_with("~/") || p == "~" {
        if let Some(home) = dirs_home() {
            return p.replacen('~', &home, 1);
        }
    }
    p.to_string()
}

fn dirs_home() -> Option<String> {
    std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).ok()
}
