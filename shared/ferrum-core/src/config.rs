use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::Path;
use crate::paths::is_within;
use crate::traits::RemoteError;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProtocolType {
    Sftp,
    Ftp,
    Ftps,
    Webdav,
    S3,
    Local,
    Ferrous,
}

/// Replaces every `${NAME}` in `value` with the environment variable `NAME`.
/// Lets a config file reference a secret held elsewhere (a systemd
/// `EnvironmentFile`, a container secret) instead of storing it. An unset
/// variable is left as written and warned about, so a typo shows up in the
/// log rather than as a silently empty password.
pub fn expand_env(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => {
                        tracing::warn!("Config references ${{{}}} but that environment variable is not set", name);
                        out.push_str(&rest[start..start + 2 + end + 1]);
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

#[derive(Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    pub password: Option<String>,
    pub private_key: Option<String>,
    pub private_key_path: Option<String>,
    pub passphrase: Option<String>,
    pub secret_key: Option<String>,
}

impl AuthConfig {
    /// Path to a private key file, when key auth is configured.
    ///
    /// Both spellings hold a filesystem path, not key material.
    /// `private_key_path` is what the example configs document;
    /// `private_key` is the older name that existing configs already use, and
    /// the only one the SSH paths used to read. Accepting both means neither
    /// spelling silently falls through to password auth.
    pub fn key_path(&self) -> Option<&str> {
        self.private_key_path
            .as_deref()
            .or(self.private_key.as_deref())
            .filter(|p| !p.trim().is_empty())
    }

    /// The same config with `${VAR}` references in its secrets replaced by
    /// the environment. Only ever applied to the copy handed to a backend:
    /// the config that gets written back to disk keeps the references.
    pub fn with_secrets_resolved(&self) -> AuthConfig {
        let resolve = |v: &Option<String>| v.as_deref().map(expand_env);
        AuthConfig {
            password: resolve(&self.password),
            private_key: self.private_key.clone(),
            private_key_path: self.private_key_path.clone(),
            passphrase: resolve(&self.passphrase),
            secret_key: resolve(&self.secret_key),
        }
    }
}

impl fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthConfig")
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .field("private_key", &self.private_key.as_ref().map(|_| "[REDACTED]"))
            .field("private_key_path", &self.private_key_path)
            .field("passphrase", &self.passphrase.as_ref().map(|_| "[REDACTED]"))
            .field("secret_key", &self.secret_key.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    pub name: String,
    pub protocol: ProtocolType,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub username: String,
    pub auth: AuthConfig,
    pub bucket: Option<String>,
    pub region: Option<String>,
    pub endpoint: Option<String>,
    pub use_ssl: Option<bool>,
    #[serde(default)]
    pub base_path: Option<String>,
    pub agent_pubkey: Option<String>,
}

impl RemoteConfig {
    /// A copy for handing to a backend, with secret references resolved.
    pub fn with_secrets_resolved(&self) -> RemoteConfig {
        RemoteConfig { auth: self.auth.with_secrets_resolved(), ..self.clone() }
    }
}

fn default_port() -> u16 {
    22
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerAuthConfig {
    pub enabled: bool,
    pub username: Option<String>,
    pub password_hash: Option<String>,
    pub totp_secret: Option<String>,
    pub session_secret: Option<String>,
    /// Mark the session cookie `Secure` so browsers only send it over HTTPS.
    /// Unset means "when Ferrite itself terminates TLS"; set it to `true`
    /// explicitly when a reverse proxy in front terminates TLS instead.
    #[serde(default)]
    pub secure_cookies: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    User,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathGrant {
    pub remote: String,
    /// Root of the grant; "/" (or empty) means the whole remote.
    pub path: String,
    #[serde(default)]
    pub write: bool,
}

impl PathGrant {
    /// Whether this grant's root covers `path`.
    ///
    /// Both sides are lexically normalized (see [`crate::paths`]) so `..`
    /// cannot climb out of the grant, and a `/`-boundary is required so a
    /// grant on "/foo" does not also cover "/foobar".
    pub fn covers(&self, path: &str) -> bool {
        is_within(&self.path, path)
    }
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Serialize, Deserialize)]
pub struct User {
    pub username: String,
    pub password_hash: String,
    pub role: Role,
    #[serde(default)]
    pub totp_secret: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Gates access to the SSH terminal feature.
    #[serde(default)]
    pub allow_shell: bool,
    #[serde(default)]
    pub permissions: Vec<PathGrant>,
}

impl fmt::Debug for User {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("User")
            .field("username", &self.username)
            .field("password_hash", &"[REDACTED]")
            .field("role", &self.role)
            .field("totp_secret", &self.totp_secret.as_ref().map(|_| "[REDACTED]"))
            .field("enabled", &self.enabled)
            .field("allow_shell", &self.allow_shell)
            .field("permissions", &self.permissions)
            .finish()
    }
}

impl User {
    /// Whether this user may access `path` on `remote`; admins always may.
    /// When `write` is true, the covering grant must also allow writes.
    pub fn can_access(&self, remote: &str, path: &str, write: bool) -> bool {
        if self.role == Role::Admin {
            return true;
        }
        self.permissions
            .iter()
            .any(|g| g.remote == remote && g.covers(path) && (!write || g.write))
    }

    /// Whether this user may use `remote` at all, for anything.
    pub fn can_use_remote(&self, remote: &str) -> bool {
        self.role == Role::Admin || self.permissions.iter().any(|g| g.remote == remote)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TlsConfig {
    pub enabled: bool,
    pub cert_path: Option<String>,
    pub key_path: Option<String>,
}

/// Request-size ceilings. Everything a browser sends Ferrite in one request
/// body (an upload, an editor save) is bounded by these; file *downloads*
/// are streamed and unbounded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Limits {
    /// Largest request body accepted, in MiB. Raise it if you routinely
    /// upload big archives through the UI; it costs nothing but memory
    /// during that one request, since uploads stream to the backend.
    #[serde(default = "default_max_upload_mb")]
    pub max_upload_mb: u64,
}

fn default_max_upload_mb() -> u64 {
    50
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_upload_mb: default_max_upload_mb() }
    }
}

impl Limits {
    pub fn max_upload_bytes(&self) -> usize {
        (self.max_upload_mb as usize).saturating_mul(1024 * 1024)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default)]
    pub server_auth: ServerAuthConfig,
    #[serde(default)]
    pub tls: TlsConfig,
    #[serde(default)]
    pub limits: Limits,
    /// Trust `X-Forwarded-For` for the client address used by login rate
    /// limiting. Only enable this behind a reverse proxy that overwrites the
    /// header; otherwise any client can pick its own address.
    #[serde(default)]
    pub trust_proxy_headers: bool,
    pub remotes: Vec<RemoteConfig>,
    #[serde(default)]
    pub terminal: Option<RemoteConfig>,
    #[serde(default)]
    pub users: Vec<User>,
}

fn default_bind() -> String {
    "127.0.0.1:8080".to_string()
}

impl Config {
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, RemoteError> {
        let content = fs::read_to_string(&path)
            .map_err(RemoteError::Io)?;
        let mut config: Config = serde_yaml::from_str(&content)
            .map_err(|e| RemoteError::OperationFailed(format!("Invalid config YAML: {}", e)))?;

        // One-time migration: fold the legacy single-admin `server_auth` block into
        // the `users` list so old configs keep working after upgrading, without
        // re-running on every subsequent load.
        if config.server_auth.enabled && config.users.is_empty() {
            if let Some(hash) = config.server_auth.password_hash.clone().filter(|h| !h.is_empty()) {
                config.users.push(User {
                    username: config.server_auth.username.clone().unwrap_or_else(|| "admin".to_string()),
                    password_hash: hash,
                    role: Role::Admin,
                    totp_secret: config.server_auth.totp_secret.clone(),
                    enabled: true,
                    allow_shell: true,
                    permissions: Vec::new(),
                });
                config.save_to_file(&path)?;
            }
        }

        Ok(config)
    }

    pub fn get_user(&self, username: &str) -> Option<&User> {
        self.users.iter().find(|u| u.username == username)
    }

    /// Writes the config atomically (temp file, then rename) so a crash
    /// mid-write cannot leave a truncated file behind, and owner-only on
    /// Unix, because the file holds remote credentials and password hashes.
    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<(), RemoteError> {
        let path = path.as_ref();
        let yaml = serde_yaml::to_string(self)
            .map_err(|e| RemoteError::OperationFailed(format!("Failed to serialize config: {}", e)))?;

        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "config".to_string());
        let tmp = path.with_file_name(format!(".{}.tmp-{}", file_name, std::process::id()));

        fs::write(&tmp, yaml).map_err(RemoteError::Io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600)).map_err(RemoteError::Io)?;
        }
        if let Err(e) = fs::rename(&tmp, path) {
            let _ = fs::remove_file(&tmp);
            return Err(RemoteError::Io(e));
        }
        Ok(())
    }

    pub fn get_remote(&self, name: &str) -> Option<&RemoteConfig> {
        self.remotes.iter().find(|r| r.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(remote: &str, path: &str, write: bool) -> PathGrant {
        PathGrant { remote: remote.into(), path: path.into(), write }
    }

    fn user(perms: Vec<PathGrant>) -> User {
        User {
            username: "u".into(),
            password_hash: String::new(),
            role: Role::User,
            totp_secret: None,
            enabled: true,
            allow_shell: false,
            permissions: perms,
        }
    }

    #[test]
    fn grant_covers_its_subtree_only() {
        let g = grant("r", "/srv/data", false);
        assert!(g.covers("/srv/data"));
        assert!(g.covers("/srv/data/a"));
        assert!(!g.covers("/srv/database"));
        assert!(!g.covers("/srv"));
    }

    #[test]
    fn grant_is_not_fooled_by_traversal() {
        let g = grant("r", "/srv/data", false);
        assert!(!g.covers("/srv/data/../../etc/passwd"));
        assert!(!g.covers("/srv/data/..\\..\\etc\\passwd"));
    }

    #[test]
    fn empty_or_root_grant_covers_everything() {
        assert!(grant("r", "", false).covers("/anything"));
        assert!(grant("r", "/", false).covers("/anything"));
    }

    #[test]
    fn write_requires_a_write_grant_on_the_same_remote() {
        let u = user(vec![grant("r", "/srv", false), grant("w", "/srv", true)]);
        assert!(u.can_access("r", "/srv/x", false));
        assert!(!u.can_access("r", "/srv/x", true));
        assert!(u.can_access("w", "/srv/x", true));
        assert!(!u.can_access("other", "/srv/x", false));
        assert!(u.can_use_remote("r"));
        assert!(!u.can_use_remote("other"));
    }

    #[test]
    fn admins_bypass_grants() {
        let mut u = user(vec![]);
        u.role = Role::Admin;
        assert!(u.can_access("any", "/etc/shadow", true));
        assert!(u.can_use_remote("any"));
    }

    #[test]
    fn env_references_are_expanded_only_in_the_resolved_copy() {
        std::env::set_var("FERRUM_TEST_PW", "s3cret");
        let auth = AuthConfig {
            password: Some("${FERRUM_TEST_PW}".into()),
            private_key: None,
            private_key_path: None,
            passphrase: Some("plain".into()),
            secret_key: Some("${FERRUM_TEST_MISSING_VAR}".into()),
        };
        let resolved = auth.with_secrets_resolved();
        assert_eq!(resolved.password.as_deref(), Some("s3cret"));
        assert_eq!(resolved.passphrase.as_deref(), Some("plain"));
        // Unknown variable: left as written rather than blanked.
        assert_eq!(resolved.secret_key.as_deref(), Some("${FERRUM_TEST_MISSING_VAR}"));
        // The original is untouched, so saving the config never writes the secret.
        assert_eq!(auth.password.as_deref(), Some("${FERRUM_TEST_PW}"));
    }

    #[test]
    fn expand_env_handles_text_around_references() {
        std::env::set_var("FERRUM_TEST_HOST", "db");
        assert_eq!(expand_env("pg://${FERRUM_TEST_HOST}:5432/x"), "pg://db:5432/x");
        assert_eq!(expand_env("no refs"), "no refs");
        assert_eq!(expand_env("unterminated ${X"), "unterminated ${X");
    }

    #[test]
    fn limits_default_to_fifty_mib_and_old_configs_still_parse() {
        let cfg: Config = serde_yaml::from_str("remotes: []\n").unwrap();
        assert_eq!(cfg.limits.max_upload_mb, 50);
        assert_eq!(cfg.limits.max_upload_bytes(), 50 * 1024 * 1024);
        assert!(!cfg.trust_proxy_headers);
        assert!(cfg.server_auth.secure_cookies.is_none());
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join(format!("ferrum-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ferrite.yaml");
        let cfg: Config = serde_yaml::from_str("remotes: []\n").unwrap();
        cfg.save_to_file(&path).unwrap();
        assert!(path.is_file());
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(leftovers.len(), 1, "temp file must be renamed away");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
