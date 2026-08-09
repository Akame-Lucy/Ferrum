use ferrum_core::Capability;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
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
                rows: rows as u16,
                cols: cols as u16,
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

/// Spawns the platform default shell in a pty scoped to `capability`'s first
/// allowed path (when that's not the unrestricted `"/"`), and starts two
/// dedicated OS threads pumping its blocking reader/writer against the
/// returned mpsc channels, mirroring `ferrite-pty::SshPtySession`'s shape.
pub fn spawn(
    capability: &Capability,
    cols: u32,
    rows: u32,
) -> Result<SpawnedPty, String> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows: rows as u16, cols: cols as u16, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| e.to_string())?;

    let mut cmd = CommandBuilder::new(default_shell());
    if let Some(root) = capability.allowed_paths.first() {
        if root != "/" {
            cmd.cwd(root);
        }
    }

    let mut child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
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
