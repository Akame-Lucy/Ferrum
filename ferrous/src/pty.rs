use ferrum_core::Capability;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// A handle to a live pty's master side, cheap to `Clone` so both the input
/// and resize paths can reach it without owning it.
#[derive(Clone)]
pub struct PtyHandle {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
}

impl PtyHandle {
    pub fn resize(&self, cols: u32, rows: u32) {
        if let Ok(master) = self.master.lock() {
            let _ = master.resize(PtySize {
                rows: rows.clamp(1, 500) as u16,
                cols: cols.clamp(1, 1000) as u16,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }
}

/// A spawned pty's handle plus the channels carrying bytes to and from the
/// shell: `Sender` feeds stdin, `Receiver` yields stdout/stderr.
type SpawnedPty = (PtyHandle, mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>);

fn default_shell() -> String {
    if cfg!(windows) {
        "powershell.exe".to_string()
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
}

/// Environment variables a shell needs to behave, and nothing else. The
/// agent's own environment may hold things a remote user has no business
/// reading (a token from the unit file, a proxy password in `https_proxy`),
/// so the shell starts from this list rather than inheriting everything.
const PASSTHROUGH_ENV: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LANGUAGE", "TZ",
    // Windows needs these to find its own binaries and profile.
    "SYSTEMROOT", "SYSTEMDRIVE", "WINDIR", "COMSPEC", "PATHEXT", "TEMP", "TMP",
    "USERPROFILE", "USERNAME", "APPDATA", "LOCALAPPDATA", "PROGRAMDATA", "HOMEDRIVE", "HOMEPATH",
];

fn scrub_environment(cmd: &mut CommandBuilder) {
    cmd.env_clear();
    for (key, value) in std::env::vars_os() {
        let name = key.to_string_lossy().to_ascii_uppercase();
        if PASSTHROUGH_ENV.contains(&name.as_str()) || name.starts_with("LC_") {
            cmd.env(key, value);
        }
    }
    cmd.env("TERM", "xterm-256color");
}

/// Spawns a shell in a pty scoped to `capability`: its working directory is
/// the first allowed path (when that's not the unrestricted `"/"`), its
/// binary is the per-client `shell` override when one is configured, and
/// its environment is reduced to what a shell needs. Two dedicated OS
/// threads pump the blocking reader/writer against the returned mpsc
/// channels, mirroring `ferrite-pty::SshPtySession`'s shape.
pub fn spawn(
    capability: &Capability,
    cols: u32,
    rows: u32,
) -> Result<SpawnedPty, String> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: rows.clamp(1, 500) as u16,
            cols: cols.clamp(1, 1000) as u16,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| e.to_string())?;

    let shell = capability
        .shell
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(default_shell);
    let mut cmd = CommandBuilder::new(&shell);
    scrub_environment(&mut cmd);

    if let Some(root) = capability.allowed_paths.first() {
        // A missing directory here is not fatal, but it must not pass
        // silently: Windows ignores an invalid working directory at process
        // creation, so the shell would come up in the user's profile instead
        // and look like the capability was never applied.
        if root != "/" {
            if Path::new(root).is_dir() {
                cmd.cwd(root);
            } else {
                tracing::warn!(
                    "allowed_paths[0] {:?} is not an existing directory; starting the shell in \
                     the agent's working directory instead. Create it to control where sessions open.",
                    root
                );
            }
        }
    }

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("failed to start shell {:?}: {}", shell, e))?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
    let mut writer = pair.master.take_writer().map_err(|e| e.to_string())?;
    let master = Arc::new(Mutex::new(pair.master));

    let (tx_in, mut rx_in) = mpsc::channel::<Vec<u8>>(128);
    let (tx_out, rx_out) = mpsc::channel::<Vec<u8>>(128);

    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx_out.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    std::thread::spawn(move || {
        while let Some(data) = rx_in.blocking_recv() {
            if writer.write_all(&data).is_err() {
                break;
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    });

    Ok((PtyHandle { master }, tx_in, rx_out))
}
