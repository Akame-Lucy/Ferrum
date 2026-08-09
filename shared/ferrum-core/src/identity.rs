use snow::{Builder, Keypair};
use std::fs;
use std::io;
use std::path::Path;

const NOISE_PATTERN: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";

/// Loads a persistent Noise static keypair from `path`, generating and saving
/// one on first run. Stored as raw `private (32 bytes) || public (32 bytes)`
/// so no key-derivation logic is needed on load.
pub fn load_or_generate_keypair(path: &Path) -> io::Result<Keypair> {
    if let Ok(bytes) = fs::read(path) {
        if bytes.len() == 64 {
            return Ok(Keypair {
                private: bytes[..32].to_vec(),
                public: bytes[32..].to_vec(),
            });
        }
    }

    let builder = Builder::new(NOISE_PATTERN.parse().unwrap());
    let keypair = builder
        .generate_keypair()
        .map_err(|e| io::Error::other(format!("keypair generation failed: {}", e)))?;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let mut bytes = keypair.private.clone();
    bytes.extend_from_slice(&keypair.public);
    fs::write(path, &bytes)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(path, perms)?;
    }

    Ok(keypair)
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[allow(clippy::manual_is_multiple_of)]
pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.is_empty() || s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}
