use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::Path;
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

#[derive(Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    pub password: Option<String>,
    pub private_key: Option<String>,
    pub private_key_path: Option<String>,
    pub passphrase: Option<String>,
    pub secret_key: Option<String>,
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

fn normalize_grant_path(p: &str) -> String {
    let s = p.replace('\\', "/");
    if s.is_empty() {
        return "/".to_string();
    }
    let trimmed = s.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

impl PathGrant {
    /// Whether this grant's root covers `path`, requiring a `/`-boundary match
    /// so a grant on "/foo" does not also cover "/foobar".
    pub fn covers(&self, path: &str) -> bool {
        let grant = normalize_grant_path(&self.path);
        if grant == "/" {
            return true;
        }
        let target = normalize_grant_path(path);
        target == grant || target.starts_with(&format!("{}/", grant))
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
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TlsConfig {
    pub enabled: bool,
    pub cert_path: Option<String>,
    pub key_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default)]
    pub server_auth: ServerAuthConfig,
    #[serde(default)]
    pub tls: TlsConfig,
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

    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<(), RemoteError> {
        let yaml = serde_yaml::to_string(self)
            .map_err(|e| RemoteError::OperationFailed(format!("Failed to serialize config: {}", e)))?;
        fs::write(path, yaml).map_err(RemoteError::Io)?;
        Ok(())
    }

    pub fn get_remote(&self, name: &str) -> Option<&RemoteConfig> {
        self.remotes.iter().find(|r| r.name == name)
    }
}
