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

/// Length of a Noise static key. Both halves of the keypair are X25519 keys.
pub const KEY_LEN: usize = 32;

/// Parses a hex-encoded Noise public key, rejecting anything that is not
/// exactly [`KEY_LEN`] bytes and saying why.
///
/// Distinct from [`from_hex`] in that it cannot be confused with "absent".
/// A pinned key that silently resolves to `None` is worse than no pin at all:
/// it reads as configured while enforcing nothing, so a single mistyped
/// character turns identity pinning off. Callers must be able to tell an
/// unset key from a malformed one, and refuse to run on the latter.
pub fn parse_public_key(s: &str) -> Result<Vec<u8>, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("key is empty".to_string());
    }

    let bytes = from_hex(trimmed).ok_or_else(|| {
        format!(
            "not valid hex: {} characters, expected {}",
            trimmed.len(),
            KEY_LEN * 2
        )
    })?;

    if bytes.len() != KEY_LEN {
        return Err(format!(
            "wrong length: {} bytes ({} hex characters), expected {} bytes ({} hex characters)",
            bytes.len(),
            trimmed.len(),
            KEY_LEN,
            KEY_LEN * 2
        ));
    }

    Ok(bytes)
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

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "6229fdff67e174bf6e7061d08a78347e6d60607188911ef9e2e7dd215be38900";

    #[test]
    fn accepts_a_full_length_key() {
        assert_eq!(parse_public_key(VALID).unwrap().len(), KEY_LEN);
    }

    #[test]
    fn tolerates_surrounding_whitespace() {
        let padded = format!("  {}\n", VALID);
        assert_eq!(parse_public_key(&padded).unwrap(), parse_public_key(VALID).unwrap());
    }

    /// The regression this whole module exists for: a key that lost its first
    /// character during a copy/paste used to parse as `None`, which callers
    /// then treated as "no pin configured" and connected unpinned.
    #[test]
    fn rejects_a_key_missing_one_character() {
        let truncated = &VALID[1..];
        let err = parse_public_key(truncated).expect_err("a 63-character key must not parse");
        assert!(err.contains("63"), "error should report the actual length, got: {err}");
    }

    #[test]
    fn rejects_a_key_that_is_short_but_still_even_length() {
        // Parses as hex cleanly, so only the length check catches it.
        let short = &VALID[..VALID.len() - 2];
        let err = parse_public_key(short).expect_err("a 31-byte key must not parse");
        assert!(err.contains("wrong length"), "got: {err}");
    }

    #[test]
    fn rejects_non_hex_and_empty_keys() {
        assert!(parse_public_key("zz").is_err());
        assert!(parse_public_key("").is_err());
        assert!(parse_public_key("   ").is_err());
    }

    #[test]
    fn round_trips_through_to_hex() {
        let bytes = parse_public_key(VALID).unwrap();
        assert_eq!(to_hex(&bytes), VALID);
    }
}
