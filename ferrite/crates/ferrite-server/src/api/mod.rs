//! HTTP API. Split by concern: `auth` (sessions, users, remotes, settings),
//! `files` (everything that touches a backend), `git`, `zip`, and `ws`
//! (terminal, collaboration, events). Shared state and helpers live here.

mod auth;
mod files;
mod git;
mod ws;
mod zip;

pub use auth::*;
pub use files::*;
pub use git::*;
pub use ws::*;
pub use zip::*;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tower_cookies::Cookies;

use ferrite_core::{
    Config, FerrousBackend, FtpBackend, LocalBackend, PathGrant, ProtocolType, RemoteConfig,
    RemoteError, RemoteFilesystem, Role, S3Backend, SftpBackend, WebDavBackend,
};

pub(crate) const SESSION_COOKIE: &str = "ferrite_session";
const SESSION_EXPIRY: Duration = Duration::from_secs(86400);
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);
const MAX_LOGIN_ATTEMPTS: u32 = 5;
/// Past this many tracked clients or sessions, expired entries are swept
/// before a new one is added, so neither map can grow without bound.
const MAX_TRACKED_LOGIN_KEYS: usize = 4096;
const MAX_TRACKED_SESSIONS: usize = 10_000;
pub(crate) const MIN_PASSWORD_LEN: usize = 8;

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum PluginEvent {
    FileOpened { remote: String, path: String },
    FileSaved { remote: String, path: String },
    FileClosed { remote: String, path: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSettings {
    pub accent_color: String,
}

impl Default for UserSettings {
    fn default() -> Self {
        Self { accent_color: "#ffffff".to_string() }
    }
}

pub struct RateLimitInfo {
    pub count: u32,
    pub window_start: Instant,
}

pub struct SessionInfo {
    pub username: String,
    pub last_seen: Instant,
}

/// The identity + permission grants resolved from a request's session cookie.
/// Handlers use this (via `AppState::current_user`) instead of touching
/// `config.users` directly.
#[derive(Clone)]
pub struct AuthedUser {
    pub username: String,
    pub role: Role,
    pub allow_shell: bool,
    pub permissions: Vec<PathGrant>,
}

impl AuthedUser {
    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }

    pub fn can_access(&self, remote: &str, path: &str, write: bool) -> bool {
        if self.is_admin() {
            return true;
        }
        self.permissions
            .iter()
            .any(|g| g.remote == remote && g.covers(path) && (!write || g.write))
    }

    pub fn can_use_remote(&self, remote: &str) -> bool {
        self.is_admin() || self.permissions.iter().any(|g| g.remote == remote)
    }
}

pub struct AppState {
    pub config: Mutex<Config>,
    pub config_path: PathBuf,
    pub client_keypair: Arc<snow::Keypair>,
    pub backends: Mutex<HashMap<String, Arc<dyn RemoteFilesystem>>>,
    pub event_tx: broadcast::Sender<PluginEvent>,
    pub collab_rooms: Mutex<HashMap<String, broadcast::Sender<String>>>,
    pub settings: Mutex<UserSettings>,
    pub active_sessions: Mutex<HashMap<String, SessionInfo>>,
    pub login_attempts: Mutex<HashMap<String, RateLimitInfo>>,
    /// Whether the session cookie carries the `Secure` flag.
    pub secure_cookies: bool,
    /// Whether `X-Forwarded-For` is believed for the client address.
    pub trust_proxy_headers: bool,
}

/// Resolves a Ferrous remote's pinned agent key.
///
/// `None` means the operator set no pin and accepts trust-on-first-use.
/// A pin that is present but unparseable is an error, never a silent `None`:
/// dropping to unpinned because one character was mistyped is the failure mode
/// this function exists to prevent.
pub(crate) fn resolve_agent_pin(remote: &RemoteConfig) -> Result<Option<Vec<u8>>, RemoteError> {
    match remote.agent_pubkey.as_deref() {
        None => Ok(None),
        Some(raw) if raw.trim().is_empty() => Ok(None),
        Some(raw) => ferrum_core::parse_public_key(raw).map(Some).map_err(|e| {
            RemoteError::OperationFailed(format!(
                "Remote '{}' has an invalid agent_pubkey ({}). Fix it or remove the line to \
                 connect unpinned; it will not be ignored.",
                remote.name, e
            ))
        }),
    }
}

fn create_default_local_remote() -> RemoteConfig {
    RemoteConfig {
        name: "local".into(),
        protocol: ProtocolType::Local,
        host: "localhost".into(),
        port: 0,
        username: "local".into(),
        auth: ferrite_core::AuthConfig {
            password: None,
            private_key: None,
            private_key_path: None,
            passphrase: None,
            secret_key: None,
        },
        bucket: None,
        region: None,
        endpoint: None,
        use_ssl: None,
        base_path: None,
        agent_pubkey: None,
    }
}

/// The one place a `RemoteConfig` becomes a live backend. Secrets written
/// as `${VAR}` are resolved here, on the copy the backend keeps, so the
/// config that gets persisted never contains them.
pub(crate) fn build_backend(
    remote: &RemoteConfig,
    client_keypair: &Arc<snow::Keypair>,
) -> Result<Arc<dyn RemoteFilesystem>, RemoteError> {
    let resolved = remote.with_secrets_resolved();
    Ok(match resolved.protocol {
        ProtocolType::Sftp => Arc::new(SftpBackend::new(resolved)),
        ProtocolType::Ftp | ProtocolType::Ftps => Arc::new(FtpBackend::new(resolved)),
        ProtocolType::Webdav => Arc::new(WebDavBackend::new(resolved)),
        ProtocolType::S3 => Arc::new(S3Backend::new(resolved)),
        ProtocolType::Local => Arc::new(LocalBackend::new(resolved)),
        ProtocolType::Ferrous => Arc::new(FerrousBackend::new(
            resolved.host.clone(),
            resolved.port,
            client_keypair.clone(),
            resolve_agent_pin(&resolved)?,
        )),
    })
}

impl AppState {
    pub fn new(
        config: Config,
        config_path: PathBuf,
        client_keypair: Arc<snow::Keypair>,
        secure_cookies: bool,
    ) -> Result<Self, RemoteError> {
        let mut backends: HashMap<String, Arc<dyn RemoteFilesystem>> = HashMap::new();
        for remote in &config.remotes {
            backends.insert(remote.name.clone(), build_backend(remote, &client_keypair)?);
        }
        if !backends.contains_key("local") {
            backends.insert("local".to_string(), Arc::new(LocalBackend::new(create_default_local_remote())));
        }

        let (event_tx, _) = broadcast::channel(100);
        let trust_proxy_headers = config.trust_proxy_headers;

        Ok(Self {
            config: Mutex::new(config),
            config_path,
            client_keypair,
            backends: Mutex::new(backends),
            event_tx,
            collab_rooms: Mutex::new(HashMap::new()),
            settings: Mutex::new(UserSettings::default()),
            active_sessions: Mutex::new(HashMap::new()),
            login_attempts: Mutex::new(HashMap::new()),
            secure_cookies,
            trust_proxy_headers,
        })
    }

    /// Persists the in-memory config (remotes, users, settings) back to disk so
    /// changes made through the API survive a restart.
    pub fn persist_config(&self) {
        let config = self.config.lock().unwrap();
        if let Err(e) = config.save_to_file(&self.config_path) {
            tracing::error!("Failed to persist config to {:?}: {}", self.config_path, e);
        }
    }

    /// Resolves the request's session cookie to the authenticated user and their
    /// permission grants. When auth is disabled, returns an implicit full-access
    /// admin so the app works unauthenticated.
    pub fn current_user(&self, cookies: &Cookies) -> Result<AuthedUser, ApiError> {
        let unauthorized = || ApiError(RemoteError::Unauthorized("Authentication required".into()));

        // Scope the config guard: `Mutex` is not reentrant, so holding it while
        // doing anything else that may touch `self.config` deadlocks the thread.
        let auth_enabled = self.config.lock().unwrap().server_auth.enabled;
        if !auth_enabled {
            return Ok(AuthedUser {
                username: "local".to_string(),
                role: Role::Admin,
                allow_shell: true,
                permissions: Vec::new(),
            });
        }

        let token = cookies
            .get(SESSION_COOKIE)
            .map(|c| c.value().to_string())
            .ok_or_else(unauthorized)?;

        let username = {
            let mut sessions = self.active_sessions.lock().unwrap();
            let info = sessions.get_mut(&token).ok_or_else(unauthorized)?;
            if info.last_seen.elapsed() >= SESSION_EXPIRY {
                sessions.remove(&token);
                return Err(unauthorized());
            }
            info.last_seen = Instant::now();
            info.username.clone()
        };

        let config = self.config.lock().unwrap();
        let user = config
            .get_user(&username)
            .filter(|u| u.enabled)
            .ok_or_else(unauthorized)?;

        Ok(AuthedUser {
            username: user.username.clone(),
            role: user.role,
            allow_shell: user.allow_shell,
            permissions: user.permissions.clone(),
        })
    }

    /// Registers a fresh session for `username` and returns its token.
    pub fn create_session(&self, username: &str) -> String {
        let token = crate::auth::generate_session_token();
        let mut sessions = self.active_sessions.lock().unwrap();
        if sessions.len() >= MAX_TRACKED_SESSIONS {
            sessions.retain(|_, s| s.last_seen.elapsed() < SESSION_EXPIRY);
        }
        sessions.insert(token.clone(), SessionInfo { username: username.to_string(), last_seen: Instant::now() });
        token
    }

    pub fn end_session(&self, token: &str) {
        self.active_sessions.lock().unwrap().remove(token);
    }

    /// Ends every session belonging to `username`: a disabled or deleted
    /// account must not stay logged in until its cookie expires.
    pub fn end_sessions_for(&self, username: &str) {
        self.active_sessions.lock().unwrap().retain(|_, s| s.username != username);
    }

    /// The key login attempts are counted under: the client's address, from
    /// `X-Forwarded-For` only when the operator said the proxy sets it.
    /// Keying on the address rather than the username means an attacker
    /// cannot lock a real user out just by failing five logins as them.
    pub fn client_key(&self, headers: &HeaderMap, peer: SocketAddr) -> String {
        if self.trust_proxy_headers {
            if let Some(forwarded) = headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split(',').next())
                .map(str::trim)
                .filter(|v| !v.is_empty())
            {
                return forwarded.to_string();
            }
        }
        peer.ip().to_string()
    }

    pub fn check_rate_limit(&self, client_key: &str) -> Result<(), ApiError> {
        let mut attempts = self.login_attempts.lock().unwrap();
        if let Some(info) = attempts.get_mut(client_key) {
            if info.window_start.elapsed() > RATE_LIMIT_WINDOW {
                info.count = 0;
                info.window_start = Instant::now();
            } else if info.count >= MAX_LOGIN_ATTEMPTS {
                return Err(ApiError(RemoteError::Unauthorized(
                    "Too many failed login attempts. Please wait 60 seconds.".into(),
                )));
            }
        }
        Ok(())
    }

    pub fn record_failed_attempt(&self, client_key: &str) {
        let mut attempts = self.login_attempts.lock().unwrap();
        if attempts.len() >= MAX_TRACKED_LOGIN_KEYS {
            attempts.retain(|_, info| info.window_start.elapsed() <= RATE_LIMIT_WINDOW);
        }
        let entry = attempts.entry(client_key.to_string()).or_insert_with(|| RateLimitInfo {
            count: 0,
            window_start: Instant::now(),
        });
        if entry.window_start.elapsed() > RATE_LIMIT_WINDOW {
            entry.count = 1;
            entry.window_start = Instant::now();
        } else {
            entry.count += 1;
        }
    }

    pub fn reset_rate_limit(&self, client_key: &str) {
        self.login_attempts.lock().unwrap().remove(client_key);
    }

    pub fn get_backend(&self, name: &str) -> Result<Arc<dyn RemoteFilesystem>, RemoteError> {
        let backends = self.backends.lock().unwrap();
        if let Some(b) = backends.get(name) {
            return Ok(b.clone());
        }
        if name == "local" || name.is_empty() {
            return Ok(Arc::new(LocalBackend::new(create_default_local_remote())));
        }
        Err(RemoteError::RemoteNotFound(name.to_string()))
    }

    pub fn get_or_create_collab_room(&self, room_id: &str) -> broadcast::Sender<String> {
        let mut rooms = self.collab_rooms.lock().unwrap();
        // Rooms nobody is in any more are just a key holding a dead sender.
        rooms.retain(|_, tx| tx.receiver_count() > 0);
        if let Some(tx) = rooms.get(room_id) {
            tx.clone()
        } else {
            let (tx, _) = broadcast::channel(100);
            rooms.insert(room_id.to_string(), tx.clone());
            tx
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

pub struct ApiError(pub RemoteError);

impl From<RemoteError> for ApiError {
    fn from(err: RemoteError) -> Self {
        ApiError(err)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match self.0 {
            RemoteError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            RemoteError::RemoteNotFound(m) => (StatusCode::BAD_REQUEST, m),
            RemoteError::InvalidInput(m) => (StatusCode::BAD_REQUEST, m),
            RemoteError::AuthFailed(m) => (StatusCode::UNAUTHORIZED, m),
            RemoteError::Unauthorized(m) => (StatusCode::UNAUTHORIZED, m),
            RemoteError::Forbidden(m) => (StatusCode::FORBIDDEN, m),
            RemoteError::ConnectionFailed(m) => (StatusCode::BAD_GATEWAY, m),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, self.0.to_string()),
        };
        (status, Json(ErrorResponse { error: msg })).into_response()
    }
}

pub(crate) fn invalid(msg: impl Into<String>) -> ApiError {
    ApiError(RemoteError::InvalidInput(msg.into()))
}

pub(crate) fn require_admin(state: &AppState, cookies: &Cookies) -> Result<AuthedUser, ApiError> {
    let user = state.current_user(cookies)?;
    if !user.is_admin() {
        return Err(ApiError(RemoteError::Forbidden("Admin access required".into())));
    }
    Ok(user)
}

/// Where `path` lives on this machine's disk, resolved through the "local"
/// backend so its base_path applies. For handlers that shell out or open
/// files directly (git, ripgrep, zip) instead of going through a backend.
pub(crate) fn local_disk_path(state: &AppState, path: &str) -> Result<PathBuf, ApiError> {
    state.get_backend("local")?.local_path(path).ok_or_else(|| {
        ApiError(RemoteError::OperationFailed(
            "The 'local' remote is not a directory on this machine".into(),
        ))
    })
}

pub(crate) fn role_str(role: Role) -> String {
    match role {
        Role::Admin => "admin".to_string(),
        Role::User => "user".to_string(),
    }
}

/// Checks a password against the account policy shared by every place a
/// password can be set.
pub(crate) fn check_password_policy(password: &str) -> Result<(), ApiError> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(invalid(format!("Password must be at least {} characters long", MIN_PASSWORD_LEN)));
    }
    Ok(())
}

/// The last component of a client-supplied file name, with anything that
/// could steer it into another directory removed. `None` when nothing
/// usable is left.
pub(crate) fn safe_file_name(raw: &str) -> Option<String> {
    let name = raw.replace('\\', "/");
    let base = name.rsplit('/').next().unwrap_or("").trim();
    if base.is_empty() || base == "." || base == ".." || base.contains('\0') {
        return None;
    }
    Some(base.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_file_name_keeps_only_the_base_name() {
        assert_eq!(safe_file_name("report.pdf").as_deref(), Some("report.pdf"));
        assert_eq!(safe_file_name("../../etc/cron.d/x").as_deref(), Some("x"));
        assert_eq!(safe_file_name("..\\..\\x.txt").as_deref(), Some("x.txt"));
        assert_eq!(safe_file_name(".."), None);
        assert_eq!(safe_file_name("dir/"), None);
        assert_eq!(safe_file_name("   "), None);
    }

    #[test]
    fn password_policy_counts_characters_not_bytes() {
        assert!(check_password_policy("short").is_err());
        assert!(check_password_policy("long enough").is_ok());
        assert!(check_password_policy("ééééééé").is_err());
        assert!(check_password_policy("éééééééé").is_ok());
    }
}
