use argon2::{
    password_hash::{rand_core::OsRng, PasswordHasher, SaltString},
    Argon2, PasswordHash, PasswordVerifier,
};
use rand::RngCore;
use subtle::ConstantTimeEq;

const TOTP_STEP_SECS: u64 = 30;
const TOTP_DIGITS: u32 = 6;

pub fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

pub fn verify_password(hash_str: &str, password: &str) -> bool {
    if let Ok(parsed_hash) = PasswordHash::new(hash_str) {
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed_hash)
            .is_ok()
    } else {
        false
    }
}

/// Constant-time equality for the legacy case of a plaintext `password_hash`.
/// `==` on strings returns at the first differing byte, which leaks how much
/// of a guess was right; this does not.
pub fn verify_plain(expected: &str, provided: &str) -> bool {
    expected.as_bytes().ct_eq(provided.as_bytes()).into()
}

/// Decodes an RFC 4648 base32 string, the format every authenticator app
/// shows and accepts. Case-insensitive; spaces, dashes and `=` padding are
/// ignored so a secret can be pasted the way an app displays it.
fn base32_decode(text: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut bits: u32 = 0;
    let mut nbits = 0;
    let mut out = Vec::with_capacity(text.len() * 5 / 8);
    for c in text.bytes() {
        let c = match c {
            b' ' | b'-' | b'=' => continue,
            c => c.to_ascii_uppercase(),
        };
        let val = ALPHABET.iter().position(|&a| a == c)? as u32;
        bits = (bits << 5) | val;
        nbits += 5;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
            bits &= (1 << nbits) - 1;
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// The raw HMAC key for a configured secret. Authenticator apps are given a
/// base32 string and decode it before use, so the server has to decode it
/// the same way or the two will never agree. A secret that is not valid
/// base32 is used as raw bytes, for anyone who provisioned the app with the
/// raw string.
fn totp_key(secret: &str) -> Vec<u8> {
    base32_decode(secret.trim()).unwrap_or_else(|| secret.trim().as_bytes().to_vec())
}

fn totp_at(key: &[u8], unix_secs: u64) -> String {
    totp_lite::totp_custom::<sha1::Sha1>(TOTP_STEP_SECS, TOTP_DIGITS, key, unix_secs)
}

/// Checks `code` against the current 30-second window and its neighbours on
/// either side, which is the tolerance RFC 6238 recommends for clock drift
/// and the second it takes to type a code.
pub fn verify_totp(secret: &str, code: &str) -> bool {
    let code = code.trim();
    if code.len() != TOTP_DIGITS as usize || !code.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let key = totp_key(secret);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Evaluate every window rather than returning on the first hit, so the
    // time taken does not reveal which one matched.
    let mut ok = false;
    for offset in [-1i64, 0, 1] {
        let t = now.saturating_add_signed(offset * TOTP_STEP_SECS as i64);
        let expected = totp_at(&key, t);
        ok |= bool::from(expected.as_bytes().ct_eq(code.as_bytes()));
    }
    ok
}

pub fn generate_session_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238's SHA-1 test secret, as raw ASCII and as the base32 an
    /// authenticator app would be given.
    const RFC_SECRET_RAW: &[u8] = b"12345678901234567890";
    const RFC_SECRET_B32: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";

    #[test]
    fn base32_decodes_the_rfc_vector() {
        assert_eq!(base32_decode(RFC_SECRET_B32).unwrap(), RFC_SECRET_RAW);
        // 12 characters carry 60 bits: seven whole bytes, four bits dropped.
        assert_eq!(base32_decode("gezd gnbv-gy3t====").unwrap(), b"1234567");
        assert!(base32_decode("not base32!").is_none());
        assert!(base32_decode("").is_none());
    }

    /// RFC 6238 Appendix B: at T=59 the 8-digit SHA-1 code is 94287082,
    /// so the 6-digit code is its last six digits.
    #[test]
    fn matches_the_rfc_6238_vector() {
        assert_eq!(totp_at(RFC_SECRET_RAW, 59), "287082");
        assert_eq!(totp_at(RFC_SECRET_RAW, 1111111109), "081804");
    }

    /// A base32 secret must produce the same code as the raw bytes it
    /// encodes, because that is what the authenticator app computes.
    #[test]
    fn base32_secret_yields_the_same_code_as_the_raw_key() {
        assert_eq!(totp_key(RFC_SECRET_B32), RFC_SECRET_RAW);
        assert_eq!(totp_at(&totp_key(RFC_SECRET_B32), 59), "287082");
    }

    #[test]
    fn rejects_malformed_codes_without_computing_anything() {
        assert!(!verify_totp(RFC_SECRET_B32, ""));
        assert!(!verify_totp(RFC_SECRET_B32, "12345"));
        assert!(!verify_totp(RFC_SECRET_B32, "abcdef"));
    }

    #[test]
    fn current_code_verifies_and_neighbour_windows_are_tolerated() {
        let key = totp_key(RFC_SECRET_B32);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(verify_totp(RFC_SECRET_B32, &totp_at(&key, now)));
        assert!(verify_totp(RFC_SECRET_B32, &totp_at(&key, now - TOTP_STEP_SECS)));
        assert!(verify_totp(RFC_SECRET_B32, &totp_at(&key, now + TOTP_STEP_SECS)));
        assert!(!verify_totp(RFC_SECRET_B32, &totp_at(&key, now + 3 * TOTP_STEP_SECS)));
    }

    #[test]
    fn plain_compare_agrees_with_equality() {
        assert!(verify_plain("hunter2", "hunter2"));
        assert!(!verify_plain("hunter2", "hunter3"));
        assert!(!verify_plain("hunter2", "hunter22"));
    }

    #[test]
    fn argon2_round_trip() {
        let hash = hash_password("correct horse").unwrap();
        assert!(hash.starts_with("$argon2"));
        assert!(verify_password(&hash, "correct horse"));
        assert!(!verify_password(&hash, "wrong"));
        assert!(!verify_password("not a hash", "anything"));
    }
}
