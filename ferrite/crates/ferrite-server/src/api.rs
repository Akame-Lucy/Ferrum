use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    body::{Body, Bytes},
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Multipart, Query, State,
    },
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;
use tower_cookies::{Cookie, Cookies};

use crate::auth::{generate_session_token, hash_password, verify_password, verify_totp};
use crate::permissions::authorize;
use ferrite_core::{
    Config, FerrousBackend, FtpBackend, LocalBackend, PathGrant, ProtocolType, RemoteError,
    RemoteFilesystem, Role, S3Backend, SftpBackend, WebDavBackend,
};
use ferrite_pty::SshPtySession;

const SESSION_EXPIRY: Duration = Duration::from_secs(86400);
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);
const MAX_LOGIN_ATTEMPTS: u32 = 5;

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
        Self {
            accent_color: "#ffffff".to_string(),
        }
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
}

/// Resolves a Ferrous remote's pinned agent key.
///
/// `None` means the operator set no pin and accepts trust-on-first-use.
/// A pin that is present but unparseable is an error, never a silent `None`:
/// dropping to unpinned because one character was mistyped is the failure mode
/// this function exists to prevent.
fn resolve_agent_pin(remote: &ferrum_core::config::RemoteConfig) -> Result<Option<Vec<u8>>, RemoteError> {
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

fn create_default_local_remote() -> ferrum_core::config::RemoteConfig {
    ferrum_core::config::RemoteConfig {
        name: "local".into(),
        protocol: ProtocolType::Local,
        host: "localhost".into(),
        port: 0,
        username: "local".into(),
        auth: ferrum_core::config::AuthConfig {
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

impl AppState {
    pub fn new(config: Config, config_path: PathBuf, client_keypair: Arc<snow::Keypair>) -> Result<Self, RemoteError> {
        let mut backends: HashMap<String, Arc<dyn RemoteFilesystem>> = HashMap::new();

        for remote in &config.remotes {
            match remote.protocol {
                ProtocolType::Sftp => {
                    let backend = Arc::new(SftpBackend::new(remote.clone()));
                    backends.insert(remote.name.clone(), backend);
                }
                ProtocolType::Ftp | ProtocolType::Ftps => {
                    let backend = Arc::new(FtpBackend::new(remote.clone()));
                    backends.insert(remote.name.clone(), backend);
                }
                ProtocolType::Webdav => {
                    let backend = Arc::new(WebDavBackend::new(remote.clone()));
                    backends.insert(remote.name.clone(), backend);
                }
                ProtocolType::S3 => {
                    let backend = Arc::new(S3Backend::new(remote.clone()));
                    backends.insert(remote.name.clone(), backend);
                }
                ProtocolType::Local => {
                    let backend = Arc::new(LocalBackend::new(remote.clone()));
                    backends.insert(remote.name.clone(), backend);
                }
                ProtocolType::Ferrous => {
                    let backend = Arc::new(FerrousBackend::new(
                        remote.host.clone(),
                        remote.port,
                        client_keypair.clone(),
                        resolve_agent_pin(remote)?,
                    ));
                    backends.insert(remote.name.clone(), backend);
                }
            }
        }

        if !backends.contains_key("local") {
            backends.insert("local".to_string(), Arc::new(LocalBackend::new(create_default_local_remote())));
        }

        let (event_tx, _) = broadcast::channel(100);

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

    pub fn is_authenticated(&self, cookies: &Cookies) -> bool {
        self.current_user(cookies).is_ok()
    }

    /// Resolves the request's session cookie to the authenticated user and their
    /// permission grants. When auth is disabled, returns an implicit full-access
    /// admin so the app keeps working unauthenticated, as it does today.
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
            .get("ferrite_session")
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

    pub fn check_rate_limit(&self, client_id: &str) -> Result<(), ApiError> {
        let mut attempts = self.login_attempts.lock().unwrap();
        if let Some(info) = attempts.get_mut(client_id) {
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

    pub fn record_failed_attempt(&self, client_id: &str) {
        let mut attempts = self.login_attempts.lock().unwrap();
        let entry = attempts.entry(client_id.to_string()).or_insert_with(|| RateLimitInfo {
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

    pub fn reset_rate_limit(&self, client_id: &str) {
        let mut attempts = self.login_attempts.lock().unwrap();
        attempts.remove(client_id);
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
pub struct RemoteSummary {
    pub name: String,
    pub protocol: String,
    pub host: String,
    pub port: u16,
    pub username: String,
}

#[derive(Debug, Deserialize)]
pub struct UpsertRemotePayload {
    pub name: String,
    pub protocol: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Option<String>,
    pub private_key: Option<String>,
    pub bucket: Option<String>,
    pub region: Option<String>,
    // Everything below used to be unreachable through this endpoint and was
    // written as null on every save, so editing a remote in the UI quietly
    // erased it from ferrite.yaml. An omitted field now means "leave as is".
    pub agent_pubkey: Option<String>,
    pub base_path: Option<String>,
    pub endpoint: Option<String>,
    pub use_ssl: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct FileQuery {
    pub remote: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateQuery {
    pub remote: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct RenamePayload {
    pub remote: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub remote: String,
    pub path: String,
    pub query: String,
    pub content: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct TerminalQuery {
    pub remote: Option<String>,
    pub cols: Option<u32>,
    pub rows: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct CollabQuery {
    pub remote: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct LoginPayload {
    pub username: String,
    pub password: String,
    pub totp_code: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AuthStatusResponse {
    pub auth_required: bool,
    pub authenticated: bool,
    pub username: Option<String>,
    pub role: Option<String>,
    pub permissions: Vec<PathGrant>,
    pub allow_shell: bool,
    pub version: String,
    pub codename: String,
}

fn role_str(role: Role) -> String {
    match role {
        Role::Admin => "admin".to_string(),
        Role::User => "user".to_string(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum TerminalControlMessage {
    #[serde(rename = "resize")]
    Resize { cols: u32, rows: u32 },
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
            RemoteError::AuthFailed(m) => (StatusCode::UNAUTHORIZED, m),
            RemoteError::Unauthorized(m) => (StatusCode::UNAUTHORIZED, m),
            RemoteError::Forbidden(m) => (StatusCode::FORBIDDEN, m),
            RemoteError::ConnectionFailed(m) => (StatusCode::BAD_GATEWAY, m),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, self.0.to_string()),
        };

        (status, Json(ErrorResponse { error: msg })).into_response()
    }
}

pub async fn auth_status(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
) -> Json<AuthStatusResponse> {
    let auth_required = state.config.lock().unwrap().server_auth.enabled;
    // Must run after the guard above is dropped: `current_user` locks
    // `state.config` itself, and re-locking on the same thread deadlocks.
    let user = state.current_user(&cookies).ok();

    Json(AuthStatusResponse {
        auth_required,
        authenticated: user.is_some(),
        username: user.as_ref().map(|u| u.username.clone()),
        role: user.as_ref().map(|u| role_str(u.role)),
        allow_shell: user.as_ref().map(|u| u.allow_shell).unwrap_or(false),
        permissions: user.map(|u| u.permissions).unwrap_or_default(),
        version: ferrum_core::VERSION.to_string(),
        codename: ferrum_core::VERSION_CODENAME.to_string(),
    })
}

pub async fn login_handler(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Json(payload): Json<LoginPayload>,
) -> Result<impl IntoResponse, ApiError> {
    let client_id = payload.username.clone();
    state.check_rate_limit(&client_id)?;

    let user = {
        let config = state.config.lock().unwrap();
        if !config.server_auth.enabled {
            return Ok((StatusCode::OK, Json(serde_json::json!({ "status": "auth_disabled" }))));
        }
        config.get_user(&payload.username).cloned()
    };

    let user = match user.filter(|u| u.enabled) {
        Some(u) => u,
        None => {
            tracing::warn!("Login failed: unknown or disabled username '{}'", payload.username);
            state.record_failed_attempt(&client_id);
            return Err(ApiError(RemoteError::AuthFailed("Invalid credentials".into())));
        }
    };

    let password_valid = if user.password_hash.starts_with("$argon2") {
        verify_password(user.password_hash.trim(), payload.password.trim())
    } else {
        payload.password.trim() == user.password_hash.trim()
    };

    if !password_valid {
        tracing::warn!("Login failed: password verification failed for username '{}'", payload.username);
        state.record_failed_attempt(&client_id);
        return Err(ApiError(RemoteError::AuthFailed("Invalid credentials".into())));
    }

    if let Some(secret) = user.totp_secret.clone() {
        if !secret.trim().is_empty() {
            let code = payload.totp_code.as_deref().unwrap_or_default();
            if !verify_totp(&secret, code) {
                state.record_failed_attempt(&client_id);
                return Err(ApiError(RemoteError::AuthFailed("Invalid TOTP code".into())));
            }
        }
    }

    state.reset_rate_limit(&client_id);
    let token = generate_session_token();
    state.active_sessions.lock().unwrap().insert(
        token.clone(),
        SessionInfo { username: user.username.clone(), last_seen: Instant::now() },
    );

    let mut cookie = Cookie::new("ferrite_session", token);
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_same_site(tower_cookies::cookie::SameSite::Lax);
    cookies.add(cookie);

    Ok((StatusCode::OK, Json(serde_json::json!({ "status": "authenticated", "role": role_str(user.role) }))))
}

pub async fn logout_handler(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
) -> Result<impl IntoResponse, ApiError> {
    if let Some(cookie) = cookies.get("ferrite_session") {
        let token = cookie.value();
        state.active_sessions.lock().unwrap().remove(token);
    }
    let mut cookie = Cookie::new("ferrite_session", "");
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_max_age(tower_cookies::cookie::time::Duration::ZERO);
    cookies.remove(cookie);

    Ok(StatusCode::OK)
}

pub async fn get_settings(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
) -> Result<impl IntoResponse, ApiError> {
    if !state.is_authenticated(&cookies) {
        return Err(ApiError(RemoteError::Unauthorized("Authentication required".into())));
    }
    let settings = state.settings.lock().unwrap();
    Ok(Json(settings.clone()))
}

pub async fn save_settings(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Json(payload): Json<UserSettings>,
) -> Result<impl IntoResponse, ApiError> {
    if !state.is_authenticated(&cookies) {
        return Err(ApiError(RemoteError::Unauthorized("Authentication required".into())));
    }
    let mut settings = state.settings.lock().unwrap();
    *settings = payload;
    Ok(StatusCode::OK)
}

#[derive(Debug, Deserialize)]
pub struct PasswordChangePayload {
    pub current_password: Option<String>,
    pub new_password: String,
}

pub async fn change_password_handler(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Json(payload): Json<PasswordChangePayload>,
) -> Result<impl IntoResponse, ApiError> {
    let authed = state.current_user(&cookies)?;
    if payload.new_password.trim().len() < 4 {
        return Err(ApiError(RemoteError::Unauthorized("Password must be at least 4 characters long".into())));
    }

    {
        let mut config = state.config.lock().unwrap();
        let user = config
            .users
            .iter_mut()
            .find(|u| u.username == authed.username)
            .ok_or_else(|| ApiError(RemoteError::Unauthorized("Account no longer exists".into())))?;

        let provided = payload.current_password.as_deref().unwrap_or_default();
        let valid = if user.password_hash.starts_with("$argon2") {
            verify_password(&user.password_hash, provided)
        } else {
            provided == user.password_hash
        };
        if !valid {
            return Err(ApiError(RemoteError::AuthFailed("Current password is incorrect".into())));
        }

        let new_hash = hash_password(&payload.new_password)
            .map_err(|e| RemoteError::Unauthorized(format!("Failed to hash password: {}", e)))?;
        user.password_hash = new_hash;
    }
    state.persist_config();

    Ok(StatusCode::OK)
}

pub async fn list_remotes(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    let config = state.config.lock().unwrap();
    let mut summaries: Vec<RemoteSummary> = config
        .remotes
        .iter()
        .filter(|r| user.is_admin() || user.permissions.iter().any(|g| g.remote == r.name))
        .map(|r| RemoteSummary {
            name: r.name.clone(),
            protocol: format!("{:?}", r.protocol).to_lowercase(),
            host: r.host.clone(),
            port: r.port,
            username: r.username.clone(),
        })
        .collect();

    let has_local = user.is_admin() || user.permissions.iter().any(|g| g.remote == "local");
    if has_local && !summaries.iter().any(|r| r.name == "local" || r.protocol == "local") {
        summaries.insert(0, RemoteSummary {
            name: "local".to_string(),
            protocol: "local".to_string(),
            host: "localhost".to_string(),
            port: 0,
            username: "local".to_string(),
        });
    }

    Ok(Json(summaries))
}

pub async fn upsert_remote(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Json(payload): Json<UpsertRemotePayload>,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    if !user.is_admin() {
        return Err(ApiError(RemoteError::Forbidden("Admin access required".into())));
    }
    let protocol = match payload.protocol.to_lowercase().as_str() {
        "sftp" => ProtocolType::Sftp,
        "ftp" => ProtocolType::Ftp,
        "ftps" => ProtocolType::Ftps,
        "webdav" => ProtocolType::Webdav,
        "s3" => ProtocolType::S3,
        "local" => ProtocolType::Local,
        "ferrous" => ProtocolType::Ferrous,
        _ => return Err(ApiError(RemoteError::OperationFailed("Invalid protocol".into()))),
    };

    // Saving a remote must not destroy settings this endpoint cannot express.
    // The form has no input for agent_pubkey, so writing null unconditionally
    // meant one UI edit silently turned off identity pinning for that agent.
    let existing = {
        let config = state.config.lock().unwrap();
        config.remotes.iter().find(|r| r.name == payload.name).cloned()
    };
    let previous = existing.as_ref();

    let remote_config = ferrite_core::RemoteConfig {
        name: payload.name.clone(),
        protocol: protocol.clone(),
        host: payload.host,
        port: payload.port,
        username: payload.username,
        auth: ferrite_core::AuthConfig {
            // An omitted secret keeps the stored one; an explicit empty string
            // still clears it, so a credential can be removed on purpose but
            // never by accident.
            password: payload.password.or_else(|| previous.and_then(|r| r.auth.password.clone())),
            private_key: payload.private_key.or_else(|| previous.and_then(|r| r.auth.private_key.clone())),
            private_key_path: previous.and_then(|r| r.auth.private_key_path.clone()),
            passphrase: previous.and_then(|r| r.auth.passphrase.clone()),
            secret_key: previous.and_then(|r| r.auth.secret_key.clone()),
        },
        bucket: payload.bucket.or_else(|| previous.and_then(|r| r.bucket.clone())),
        region: payload.region.or_else(|| previous.and_then(|r| r.region.clone())),
        endpoint: payload.endpoint.or_else(|| previous.and_then(|r| r.endpoint.clone())),
        use_ssl: payload.use_ssl.or_else(|| previous.and_then(|r| r.use_ssl)),
        base_path: payload.base_path.or_else(|| previous.and_then(|r| r.base_path.clone())),
        agent_pubkey: payload.agent_pubkey.or_else(|| previous.and_then(|r| r.agent_pubkey.clone())),
    };

    // Reject a bad pin here rather than storing it and failing open later.
    let agent_pin = resolve_agent_pin(&remote_config).map_err(ApiError)?;

    let backend: Arc<dyn RemoteFilesystem> = match protocol {
        ProtocolType::Sftp => Arc::new(SftpBackend::new(remote_config.clone())),
        ProtocolType::Ftp | ProtocolType::Ftps => Arc::new(FtpBackend::new(remote_config.clone())),
        ProtocolType::Webdav => Arc::new(WebDavBackend::new(remote_config.clone())),
        ProtocolType::S3 => Arc::new(S3Backend::new(remote_config.clone())),
        ProtocolType::Local => Arc::new(LocalBackend::new(remote_config.clone())),
        ProtocolType::Ferrous => Arc::new(FerrousBackend::new(
            remote_config.host.clone(),
            remote_config.port,
            state.client_keypair.clone(),
            agent_pin,
        )),
    };

    {
        let mut backends = state.backends.lock().unwrap();
        backends.insert(payload.name.clone(), backend);
    }

    {
        let mut config = state.config.lock().unwrap();
        if let Some(pos) = config.remotes.iter().position(|r| r.name == payload.name) {
            config.remotes[pos] = remote_config;
        } else {
            config.remotes.push(remote_config);
        }
    }
    state.persist_config();

    Ok(StatusCode::OK)
}

fn require_admin(state: &AppState, cookies: &Cookies) -> Result<AuthedUser, ApiError> {
    let user = state.current_user(cookies)?;
    if !user.is_admin() {
        return Err(ApiError(RemoteError::Forbidden("Admin access required".into())));
    }
    Ok(user)
}

#[derive(Debug, Serialize)]
pub struct UserSummary {
    pub username: String,
    pub role: String,
    pub enabled: bool,
    pub allow_shell: bool,
    pub permissions: Vec<PathGrant>,
}

impl From<&ferrite_core::User> for UserSummary {
    fn from(u: &ferrite_core::User) -> Self {
        Self {
            username: u.username.clone(),
            role: role_str(u.role),
            enabled: u.enabled,
            allow_shell: u.allow_shell,
            permissions: u.permissions.clone(),
        }
    }
}

fn parse_role(s: &str) -> Result<Role, ApiError> {
    match s.to_lowercase().as_str() {
        "admin" => Ok(Role::Admin),
        "user" => Ok(Role::User),
        _ => Err(ApiError(RemoteError::OperationFailed("Invalid role".into()))),
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateUserPayload {
    pub username: String,
    pub password: String,
    pub role: String,
    #[serde(default)]
    pub allow_shell: bool,
}

pub async fn list_users(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &cookies)?;
    let config = state.config.lock().unwrap();
    let users: Vec<UserSummary> = config.users.iter().map(UserSummary::from).collect();
    Ok(Json(users))
}

pub async fn create_user(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Json(payload): Json<CreateUserPayload>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &cookies)?;

    let username = payload.username.trim().to_string();
    if username.is_empty() {
        return Err(ApiError(RemoteError::OperationFailed("Username is required".into())));
    }
    if payload.password.trim().len() < 4 {
        return Err(ApiError(RemoteError::OperationFailed(
            "Password must be at least 4 characters long".into(),
        )));
    }
    let role = parse_role(&payload.role)?;
    let password_hash = hash_password(&payload.password)
        .map_err(|e| RemoteError::OperationFailed(format!("Failed to hash password: {}", e)))?;

    {
        let mut config = state.config.lock().unwrap();
        if config.users.iter().any(|u| u.username == username) {
            return Err(ApiError(RemoteError::OperationFailed("Username already exists".into())));
        }
        config.users.push(ferrite_core::User {
            username: username.clone(),
            password_hash,
            role,
            totp_secret: None,
            enabled: true,
            allow_shell: payload.allow_shell,
            permissions: Vec::new(),
        });
    }
    state.persist_config();

    Ok(StatusCode::CREATED)
}

#[derive(Debug, Deserialize)]
pub struct UpdateUserPayload {
    pub role: Option<String>,
    pub enabled: Option<bool>,
    pub allow_shell: Option<bool>,
    pub password: Option<String>,
    pub permissions: Option<Vec<PathGrant>>,
}

pub async fn update_user(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    axum::extract::Path(username): axum::extract::Path<String>,
    Json(payload): Json<UpdateUserPayload>,
) -> Result<impl IntoResponse, ApiError> {
    let admin = require_admin(&state, &cookies)?;

    {
        let mut config = state.config.lock().unwrap();
        let is_last_admin = config.users.iter().filter(|u| u.role == Role::Admin && u.enabled).count() <= 1;
        let user = config
            .users
            .iter_mut()
            .find(|u| u.username == username)
            .ok_or_else(|| ApiError(RemoteError::NotFound("User not found".into())))?;

        if let Some(role) = payload.role.as_deref() {
            let new_role = parse_role(role)?;
            if user.role == Role::Admin && new_role != Role::Admin && is_last_admin {
                return Err(ApiError(RemoteError::OperationFailed("Cannot demote the last admin".into())));
            }
            user.role = new_role;
        }
        if let Some(enabled) = payload.enabled {
            if !enabled && user.username == admin.username {
                return Err(ApiError(RemoteError::OperationFailed("Cannot disable your own account".into())));
            }
            if !enabled && user.role == Role::Admin && is_last_admin {
                return Err(ApiError(RemoteError::OperationFailed("Cannot disable the last admin".into())));
            }
            user.enabled = enabled;
        }
        if let Some(allow_shell) = payload.allow_shell {
            user.allow_shell = allow_shell;
        }
        if let Some(permissions) = payload.permissions {
            user.permissions = permissions;
        }
        if let Some(password) = payload.password.filter(|p| !p.trim().is_empty()) {
            if password.trim().len() < 4 {
                return Err(ApiError(RemoteError::OperationFailed(
                    "Password must be at least 4 characters long".into(),
                )));
            }
            user.password_hash = hash_password(&password)
                .map_err(|e| RemoteError::OperationFailed(format!("Failed to hash password: {}", e)))?;
        }
    }
    state.persist_config();

    Ok(StatusCode::OK)
}

pub async fn delete_user(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    axum::extract::Path(username): axum::extract::Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let admin = require_admin(&state, &cookies)?;
    if username == admin.username {
        return Err(ApiError(RemoteError::OperationFailed("Cannot delete your own account".into())));
    }

    {
        let mut config = state.config.lock().unwrap();
        let is_last_admin = config.users.iter().filter(|u| u.role == Role::Admin && u.enabled).count() <= 1;
        let target = config
            .users
            .iter()
            .find(|u| u.username == username)
            .ok_or_else(|| ApiError(RemoteError::NotFound("User not found".into())))?;
        if target.role == Role::Admin && is_last_admin {
            return Err(ApiError(RemoteError::OperationFailed("Cannot delete the last admin".into())));
        }
        config.users.retain(|u| u.username != username);
    }
    state.persist_config();

    Ok(StatusCode::OK)
}

pub async fn list_files(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<FileQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, &query.remote, &query.path, false)?;
    let backend = state.get_backend(&query.remote)?;
    let entries = backend.list_dir(&query.path).await?;
    Ok(Json(entries))
}

pub async fn read_file(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    headers: HeaderMap,
    Query(query): Query<FileQuery>,
) -> Result<Response, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, &query.remote, &query.path, false)?;
    let backend = state.get_backend(&query.remote)?;

    let clean = clean_path_str(&query.path);
    let p = std::path::Path::new(&clean);

    let mime = mime_guess::from_path(&query.path)
        .first_or_octet_stream()
        .to_string();

    let _ = state.event_tx.send(PluginEvent::FileOpened {
        remote: query.remote.clone(),
        path: query.path.clone(),
    });

    if p.exists() && p.is_file() {
        if let Ok(metadata) = std::fs::metadata(p) {
            let total_size = metadata.len();

            if let Some(range_header) = headers.get(header::RANGE).and_then(|h| h.to_str().ok()) {
                if let Some(spec) = range_header.strip_prefix("bytes=") {
                    let parts: Vec<&str> = spec.split('-').collect();
                    let start: u64 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
                    let end: u64 = parts
                        .get(1)
                        .and_then(|s| if s.is_empty() { None } else { s.parse().ok() })
                        .unwrap_or(total_size.saturating_sub(1));

                    if start < total_size {
                        let end = std::cmp::min(end, total_size.saturating_sub(1));
                        let chunk_size = end - start + 1;

                        use std::io::{Read, Seek, SeekFrom};
                        if let Ok(mut file) = std::fs::File::open(p) {
                            if file.seek(SeekFrom::Start(start)).is_ok() {
                                let mut buffer = vec![0u8; chunk_size as usize];
                                if file.read_exact(&mut buffer).is_ok() {
                                    let mut resp_headers = HeaderMap::new();
                                    resp_headers.insert(
                                        header::CONTENT_TYPE,
                                        mime.parse().unwrap_or_else(|_| "application/octet-stream".parse().unwrap()),
                                    );
                                    resp_headers.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
                                    resp_headers.insert(
                                        header::CONTENT_RANGE,
                                        format!("bytes {}-{}/{}", start, end, total_size).parse().unwrap(),
                                    );
                                    resp_headers.insert(
                                        header::CONTENT_LENGTH,
                                        chunk_size.to_string().parse().unwrap(),
                                    );

                                    return Ok((StatusCode::PARTIAL_CONTENT, resp_headers, Body::from(buffer)).into_response());
                                }
                            }
                        }
                    }
                }
            }

            let stream = backend.stream_file(&query.path).await?;
            let body = Body::from_stream(stream.map(|res| {
                res.map_err(|e| std::io::Error::other(e.to_string()))
            }));

            let mut resp_headers = HeaderMap::new();
            resp_headers.insert(
                header::CONTENT_TYPE,
                mime.parse().unwrap_or_else(|_| "application/octet-stream".parse().unwrap()),
            );
            resp_headers.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
            resp_headers.insert(header::CONTENT_LENGTH, total_size.to_string().parse().unwrap());

            return Ok((StatusCode::OK, resp_headers, body).into_response());
        }
    }

    let stream = backend.stream_file(&query.path).await?;
    let body = Body::from_stream(stream.map(|res| {
        res.map_err(|e| std::io::Error::other(e.to_string()))
    }));

    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(
        header::CONTENT_TYPE,
        mime.parse().unwrap_or_else(|_| "application/octet-stream".parse().unwrap()),
    );
    resp_headers.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());

    Ok((StatusCode::OK, resp_headers, body).into_response())
}

pub async fn write_file(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<FileQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, &query.remote, &query.path, true)?;
    let backend = state.get_backend(&query.remote)?;

    if let Some(if_match) = headers.get("if-match").and_then(|h| h.to_str().ok()) {
        if let Ok(existing_content) = backend.read_file(&query.path).await {
            let mut hasher = Sha256::new();
            hasher.update(&existing_content);
            let current_hash = format!("{:x}", hasher.finalize());

            if current_hash != if_match {
                return Err(ApiError(RemoteError::OperationFailed(
                    "Remote file has been modified externally".to_string(),
                )));
            }
        }
    }

    backend.write_file(&query.path, &body).await?;

    let _ = state.event_tx.send(PluginEvent::FileSaved {
        remote: query.remote.clone(),
        path: query.path.clone(),
    });

    Ok(StatusCode::OK)
}

pub async fn create_file(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<CreateQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, &query.remote, &query.path, true)?;
    let backend = state.get_backend(&query.remote)?;
    backend.create_file(&query.path).await?;
    Ok(StatusCode::CREATED)
}

pub async fn create_directory(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<CreateQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, &query.remote, &query.path, true)?;
    let backend = state.get_backend(&query.remote)?;
    backend.create_dir(&query.path).await?;
    Ok(StatusCode::CREATED)
}

pub async fn delete_entry(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<FileQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, &query.remote, &query.path, true)?;
    let backend = state.get_backend(&query.remote)?;
    backend.delete(&query.path).await?;
    Ok(StatusCode::OK)
}

pub async fn rename_entry(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Json(payload): Json<RenamePayload>,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, &payload.remote, &payload.from, true)?;
    authorize(&user, &payload.remote, &payload.to, true)?;
    let backend = state.get_backend(&payload.remote)?;
    backend.rename(&payload.from, &payload.to).await?;
    Ok(StatusCode::OK)
}

pub async fn copy_entry(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Json(payload): Json<RenamePayload>,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, &payload.remote, &payload.from, false)?;
    authorize(&user, &payload.remote, &payload.to, true)?;
    let backend = state.get_backend(&payload.remote)?;
    let content = backend.read_file(&payload.from).await?;
    backend.write_file(&payload.to, &content).await?;
    Ok(StatusCode::OK)
}

pub async fn download_file(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<FileQuery>,
) -> Result<Response, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, &query.remote, &query.path, false)?;
    let backend = state.get_backend(&query.remote)?;
    let stream = backend.stream_file(&query.path).await?;
    let body = Body::from_stream(stream);

    let filename = std::path::Path::new(&query.path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".to_string());

    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        "application/octet-stream".parse().unwrap(),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"{}\"", filename).parse().unwrap(),
    );

    Ok(response)
}

pub async fn upload_file(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<FileQuery>,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, &query.remote, &query.path, true)?;
    let backend = state.get_backend(&query.remote)?;

    while let Ok(Some(field)) = multipart.next_field().await {
        let field_name = field.file_name().unwrap_or("file").to_string();
        let target_path = if query.path.ends_with('/') || query.path.ends_with('\\') {
            format!("{}{}", query.path, field_name)
        } else {
            format!("{}/{}", query.path, field_name)
        };

        let data = field.bytes().await.map_err(|e| {
            RemoteError::Io(std::io::Error::other(e.to_string()))
        })?;

        backend.write_file(&target_path, &data).await?;
    }

    Ok(StatusCode::OK)
}

#[derive(Debug, Deserialize)]
pub struct ZipDownloadPayload {
    pub remote: String,
    pub paths: Vec<String>,
}

pub async fn download_zip(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Json(payload): Json<ZipDownloadPayload>,
) -> Result<Response, ApiError> {
    let user = state.current_user(&cookies)?;
    for path in &payload.paths {
        authorize(&user, &payload.remote, path, false)?;
    }
    let backend = state.get_backend(&payload.remote)?;

    let mut buf = Vec::new();
    {
        let mut zip_writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        for path in payload.paths {
            if let Err(err) = zip_add_path(&*backend, &mut zip_writer, &path, options).await {
                tracing::warn!("Failed to add path '{}' to zip: {}", path, err);
            }
        }
        zip_writer.finish().map_err(|e| {
            RemoteError::OperationFailed(format!("Zip finish error: {}", e))
        })?;
    }

    let body = Body::from(buf);
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        "application/zip".parse().unwrap(),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        "attachment; filename=\"ferrite_archive.zip\"".parse().unwrap(),
    );

    Ok(response)
}

async fn zip_add_path<W: std::io::Write + std::io::Seek>(
    backend: &dyn RemoteFilesystem,
    writer: &mut zip::ZipWriter<W>,
    target_path: &str,
    options: zip::write::SimpleFileOptions,
) -> Result<(), RemoteError> {
    let meta = backend.stat(target_path).await.ok();
    let is_dir = meta.as_ref().map(|m| m.is_dir).unwrap_or(false);

    if !is_dir {
        if let Ok(content) = backend.read_file(target_path).await {
            let name = std::path::Path::new(target_path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "file".to_string());
            let clean_name = name.replace('\\', "/");
            let _ = writer.start_file(&clean_name, options);
            use std::io::Write;
            let _ = writer.write_all(&content);
        }
        return Ok(());
    }

    let root_parent = std::path::Path::new(target_path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut queue = vec![target_path.to_string()];
    while let Some(dir_path) = queue.pop() {
        if let Ok(entries) = backend.list_dir(&dir_path).await {
            for entry in entries {
                if entry.is_dir {
                    queue.push(entry.path);
                } else {
                    let rel_path = if !root_parent.is_empty() && entry.path.starts_with(&root_parent) {
                        entry.path[root_parent.len()..].trim_start_matches(['/', '\\']).to_string()
                    } else {
                        entry.name.clone()
                    };
                    let clean_rel_path = rel_path.replace('\\', "/");
                    if let Ok(content) = backend.read_file(&entry.path).await {
                        let _ = writer.start_file(&clean_rel_path, options);
                        use std::io::Write;
                        let _ = writer.write_all(&content);
                    }
                }
            }
        }
    }

    Ok(())
}

pub async fn search_files(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<SearchQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, &query.remote, &query.path, false)?;
    let backend = state.get_backend(&query.remote)?;
    let content_search = query.content.unwrap_or(false);
    let matches = backend.search(&query.path, &query.query, content_search).await?;
    Ok(Json(matches))
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct GlobalSearchQuery {
    pub remote: String,
    pub path: String,
    pub query: String,
    pub case_sensitive: Option<bool>,
    pub is_regex: Option<bool>,
    pub includes: Option<String>,
    pub excludes: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SearchMatchLine {
    pub file: String,
    pub line: usize,
    pub text: String,
}

pub async fn global_search(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<GlobalSearchQuery>,
) -> Result<Json<Vec<SearchMatchLine>>, ApiError> {
    let user = state.current_user(&cookies)?;
    // This handler shells out to `rg` against the server's local filesystem
    // regardless of `query.remote`, so it is authorized against "local".
    authorize(&user, "local", &query.path, false)?;

    let clean = clean_path_str(&query.path);
    let search_dir = std::path::Path::new(&clean);
    let case_sensitive = query.case_sensitive.unwrap_or(false);
    let is_regex = query.is_regex.unwrap_or(false);

    if !search_dir.exists() {
        return Ok(Json(Vec::new()));
    }

    let target_dir = if search_dir.is_file() {
        search_dir.parent().unwrap_or(search_dir)
    } else {
        search_dir
    };

    let mut args: Vec<String> = vec![
        "--no-heading".into(),
        "--line-number".into(),
        "--color=never".into(),
        "--max-count=10".into(),
    ];

    if !case_sensitive {
        args.push("-i".into());
    }
    if !is_regex {
        args.push("-F".into());
    }

    if let Some(ref inc) = query.includes {
        for glob in inc.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
            args.push("-g".into());
            args.push(glob.to_string());
        }
    }
    if let Some(ref exc) = query.excludes {
        for glob in exc.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
            args.push("-g".into());
            args.push(format!("!{}", glob));
        }
    }

    args.push("--".into());
    args.push(query.query.clone());
    args.push(".".into());

    let output = tokio::process::Command::new("rg")
        .args(&args)
        .current_dir(target_dir)
        .output()
        .await;

    let mut results = Vec::new();
    let max_results = 200;

    if let Ok(out) = output {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if results.len() >= max_results { break; }
            let line_clean = line.trim_start_matches("./");
            if let Some(first_colon) = line_clean.find(':') {
                let file_rel = &line_clean[..first_colon];
                let rest = &line_clean[first_colon + 1..];
                if let Some(second_colon) = rest.find(':') {
                    if let Ok(ln) = rest[..second_colon].parse::<usize>() {
                        let match_text = rest[second_colon + 1..].to_string();
                        let target_base = target_dir.to_string_lossy().replace('\\', "/");
                        let full_file_path = format!("{}/{}", target_base.trim_end_matches('/'), file_rel.replace('\\', "/"));
                        results.push(SearchMatchLine {
                            file: full_file_path,
                            line: ln,
                            text: if match_text.len() > 200 { match_text[..200].to_string() } else { match_text },
                        });
                    }
                }
            }
        }
    }

    Ok(Json(results))
}

pub async fn collab_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<CollabQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, &query.remote, &query.path, false)?;
    let room_id = format!("{}:{}", query.remote, query.path);
    let room_tx = state.get_or_create_collab_room(&room_id);
    Ok(ws.on_upgrade(move |socket| handle_collab_socket(socket, room_tx)))
}

async fn handle_collab_socket(socket: WebSocket, room_tx: broadcast::Sender<String>) {
    let (mut sender, mut receiver) = socket.split();
    let mut rx = room_tx.subscribe();

    let mut send_task = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            if sender.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
    });

    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            if let Message::Text(text) = msg {
                let _ = room_tx.send(text);
            }
        }
    });

    tokio::select! {
        _ = &mut send_task => recv_task.abort(),
        _ = &mut recv_task => send_task.abort(),
    };
}

pub async fn events_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
) -> Result<impl IntoResponse, ApiError> {
    if !state.is_authenticated(&cookies) {
        return Err(ApiError(RemoteError::Unauthorized("Authentication required".into())));
    }
    let event_tx = state.event_tx.clone();
    Ok(ws.on_upgrade(move |socket| handle_events_socket(socket, event_tx)))
}

async fn handle_events_socket(socket: WebSocket, event_tx: broadcast::Sender<PluginEvent>) {
    let (mut sender, _) = socket.split();
    let mut rx = event_tx.subscribe();

    while let Ok(event) = rx.recv().await {
        if let Ok(json) = serde_json::to_string(&event) {
            if sender.send(Message::Text(json)).await.is_err() {
                break;
            }
        }
    }
}

pub async fn terminal_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<TerminalQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    if !user.allow_shell {
        return Err(ApiError(RemoteError::Forbidden("Shell access is disabled for this account".into())));
    }
    let remote_config = {
        let config = state.config.lock().unwrap();
        if let Some(terminal) = config.terminal.clone() {
            Some(terminal)
        } else if let Some(name) = query.remote.as_ref() {
            config.get_remote(name).cloned()
        } else {
            None
        }
    }.ok_or_else(|| RemoteError::OperationFailed(
        "No terminal SSH host configured. Add a 'terminal:' section to ferrite.yaml.".into(),
    ))?;

    let cols = query.cols.unwrap_or(80);
    let rows = query.rows.unwrap_or(24);

    Ok(ws.on_upgrade(move |socket| handle_terminal_socket(socket, state, remote_config, cols, rows)))
}

/// The upgrade already succeeded by this point, so the only way to tell the user
/// why the terminal is empty is to write the reason into the xterm stream.
async fn report_terminal_failure(mut socket: WebSocket, reason: &str) {
    let _ = socket
        .send(Message::Text(format!("\r\n\x1b[31m{}\x1b[0m\r\n", reason)))
        .await;
    let _ = socket.send(Message::Close(None)).await;
}

enum PtyBackend {
    Ssh(SshPtySession),
    Ferrous(ferrite_core::FerrousPtySession),
}

impl PtyBackend {
    fn resize(&self, cols: u32, rows: u32) -> Result<(), RemoteError> {
        match self {
            PtyBackend::Ssh(s) => s.resize(cols, rows),
            PtyBackend::Ferrous(s) => s.resize(cols, rows),
        }
    }
}

async fn handle_terminal_socket(
    socket: WebSocket,
    state: Arc<AppState>,
    remote_config: ferrite_core::RemoteConfig,
    cols: u32,
    rows: u32,
) {
    let host = format!("{}:{}", remote_config.host, remote_config.port);

    let (pty_session, tx_pty_in, mut rx_pty_out) = if remote_config.protocol == ProtocolType::Ferrous {
        let client_keypair = state.client_keypair.clone();
        // A malformed pin must not open an unpinned shell. Fail the session and
        // say why, rather than connecting to an agent nobody verified.
        let expected_agent_pubkey = match resolve_agent_pin(&remote_config) {
            Ok(pin) => pin,
            Err(e) => {
                tracing::warn!("Terminal Ferrous connect to {} refused: {}", host, e);
                report_terminal_failure(socket, &format!("{}", e)).await;
                return;
            }
        };
        match ferrite_core::FerrousPtySession::connect(
            remote_config.host.clone(),
            remote_config.port,
            client_keypair,
            expected_agent_pubkey,
            cols,
            rows,
        ).await {
            Ok((session, tx, rx)) => (PtyBackend::Ferrous(session), tx, rx),
            Err(e) => {
                tracing::warn!("Terminal Ferrous connect to {} failed: {}", host, e);
                report_terminal_failure(socket, &format!("Ferrous connection to {} failed: {}", host, e)).await;
                return;
            }
        }
    } else {
        match tokio::task::spawn_blocking(move || SshPtySession::connect(remote_config, cols, rows)).await {
            Ok(Ok((session, tx, rx))) => (PtyBackend::Ssh(session), tx, rx),
            Ok(Err(e)) => {
                tracing::warn!("Terminal SSH connect to {} failed: {}", host, e);
                report_terminal_failure(socket, &format!("SSH connection to {} failed: {}", host, e)).await;
                return;
            }
            Err(e) => {
                tracing::error!("Terminal connect task panicked: {}", e);
                report_terminal_failure(socket, "Internal error while opening the SSH session.").await;
                return;
            }
        }
    };

    let pty_session = Arc::new(pty_session);
    let (mut ws_sender, mut ws_receiver) = socket.split();

    let pty_session_clone = Arc::clone(&pty_session);
    let mut receive_task = tokio::spawn(async move {
        while let Some(msg_result) = ws_receiver.next().await {
            let msg = match msg_result {
                Ok(m) => m,
                Err(_) => break,
            };

            match msg {
                Message::Text(text) => {
                    if let Ok(ctrl) = serde_json::from_str::<TerminalControlMessage>(&text) {
                        match ctrl {
                            TerminalControlMessage::Resize { cols, rows } => {
                                let session = Arc::clone(&pty_session_clone);
                                let _ = tokio::task::spawn_blocking(move || session.resize(cols, rows)).await;
                            }
                        }
                    } else if tx_pty_in.send(text.into_bytes()).await.is_err() {
                        break;
                    }
                }
                Message::Binary(bytes) => {
                    if tx_pty_in.send(bytes).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    let mut send_task = tokio::spawn(async move {
        while let Some(bytes) = rx_pty_out.recv().await {
            if ws_sender.send(Message::Binary(bytes)).await.is_err() {
                break;
            }
        }
    });

    tokio::select! {
        _ = &mut receive_task => send_task.abort(),
        _ = &mut send_task => receive_task.abort(),
    };
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct GitQueryParams {
    pub remote: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct GitFileQueryParams {
    pub remote: String,
    pub path: String,
    pub file: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct GitBlameQueryParams {
    pub remote: String,
    pub path: String,
    pub file: String,
    pub line: usize,
}

#[derive(Debug, Serialize)]
pub struct FileGitStatus {
    pub path: String,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct GitStatusResponse {
    pub is_git: bool,
    pub branch: String,
    pub repo_root: String,
    pub statuses: Vec<FileGitStatus>,
}

#[derive(Debug, Serialize)]
pub struct GitDiffResponse {
    pub path: String,
    pub original: String,
    pub modified: String,
}

#[derive(Debug, Serialize)]
pub struct GitBlameResponse {
    pub line: usize,
    pub author: String,
    pub time_ago: String,
    pub summary: String,
    pub hash: String,
}

#[derive(Debug, Serialize)]
pub struct GitCommitItem {
    pub hash: String,
    pub author: String,
    pub time_ago: String,
    pub summary: String,
}

#[derive(Debug, Serialize)]
pub struct GitHistoryResponse {
    pub path: String,
    pub commits: Vec<GitCommitItem>,
}

fn clean_path_str(raw_path: &str) -> String {
    let s = raw_path.trim();
    if cfg!(windows) && s.starts_with('/') && s.len() >= 3 && s.chars().nth(2) == Some(':') {
        s[1..].to_string()
    } else {
        s.to_string()
    }
}

fn get_directory_path(raw_path: &str) -> std::path::PathBuf {
    let clean = clean_path_str(raw_path);
    let p = std::path::Path::new(&clean);
    if p.is_file() || (clean.contains('.') && !clean.ends_with('/') && !clean.ends_with('\\') && p.extension().is_some()) {
        p.parent().map(|parent| parent.to_path_buf()).unwrap_or_else(|| p.to_path_buf())
    } else {
        p.to_path_buf()
    }
}

pub async fn git_status(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<GitQueryParams>,
) -> Result<Json<GitStatusResponse>, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, "local", &query.path, false)?;

    let repo_dir = get_directory_path(&query.path);

    // Find the actual git repository root
    let toplevel_output = tokio::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(repo_dir)
        .output()
        .await;

    let repo_root = match &toplevel_output {
        Ok(out) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).trim().replace('\\', "/").to_string()
        }
        _ => {
            return Ok(Json(GitStatusResponse {
                is_git: false,
                branch: "".into(),
                repo_root: "".into(),
                statuses: vec![],
            }));
        }
    };

    let branch_output = tokio::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(&repo_root)
        .output()
        .await;

    let branch = match branch_output {
        Ok(out) if out.status.success() => {
            let b = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if b.is_empty() { "HEAD".to_string() } else { b }
        }
        _ => "HEAD".to_string(),
    };

    let status_output = tokio::process::Command::new("git")
        .args(["status", "--porcelain=v1"])
        .current_dir(&repo_root)
        .output()
        .await;

    let mut statuses = Vec::new();
    if let Ok(out) = status_output {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                if line.len() >= 4 {
                    let code = line[0..2].trim().to_string();
                    let file_path = line[3..].trim().replace('\\', "/").to_string();
                    let status_char = if code.contains('?') {
                        "U".to_string()
                    } else if code.contains('M') {
                        "M".to_string()
                    } else if code.contains('D') {
                        "D".to_string()
                    } else if code.contains('A') {
                        "A".to_string()
                    } else {
                        code
                    };
                    statuses.push(FileGitStatus {
                        path: file_path,
                        status: status_char,
                    });
                }
            }
        }
    }

    Ok(Json(GitStatusResponse {
        is_git: true,
        branch,
        repo_root: repo_root.clone(),
        statuses,
    }))
}

pub async fn git_diff(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<GitFileQueryParams>,
) -> Result<Json<GitDiffResponse>, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, "local", &query.path, false)?;
    let repo_dir = get_directory_path(&query.path);

    let show_spec = format!("HEAD:{}", query.file.replace('\\', "/"));
    let show_output = tokio::process::Command::new("git")
        .args(["show", &show_spec])
        .current_dir(&repo_dir)
        .output()
        .await;

    let original = match show_output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).to_string(),
        _ => "".to_string(),
    };

    let full_path = repo_dir.join(&query.file);
    let modified = tokio::fs::read_to_string(&full_path).await.unwrap_or_default();

    Ok(Json(GitDiffResponse {
        path: query.file,
        original,
        modified,
    }))
}

pub async fn git_blame(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<GitBlameQueryParams>,
) -> Result<Json<GitBlameResponse>, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, "local", &query.path, false)?;
    let repo_dir = get_directory_path(&query.path);
    let line_str = format!("{},{}", query.line, query.line);
    let clean_file = query.file.replace('\\', "/");

    let output = tokio::process::Command::new("git")
        .args(["blame", "-L", &line_str, "--porcelain", &clean_file])
        .current_dir(&repo_dir)
        .output()
        .await;

    let mut author = "Not Committed Yet".to_string();
    let mut summary = "Uncommitted changes".to_string();
    let mut time_ago = "now".to_string();
    let mut hash = "0000000".to_string();

    if let Ok(out) = output {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            let mut lines = text.lines();
            if let Some(first) = lines.next() {
                let parts: Vec<&str> = first.split_whitespace().collect();
                if !parts.is_empty() {
                    hash = parts[0].chars().take(7).collect();
                }
            }
            for line in lines {
                if let Some(rest) = line.strip_prefix("author ") {
                    author = rest.trim().to_string();
                } else if let Some(rest) = line.strip_prefix("summary ") {
                    summary = rest.trim().to_string();
                } else if let Some(rest) = line.strip_prefix("author-time ") {
                    if let Ok(ts) = rest.trim().parse::<u64>() {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        let diff = now.saturating_sub(ts);
                        time_ago = if diff < 60 {
                            "just now".to_string()
                        } else if diff < 3600 {
                            format!("{}m ago", diff / 60)
                        } else if diff < 86400 {
                            format!("{}h ago", diff / 3600)
                        } else {
                            format!("{}d ago", diff / 86400)
                        };
                    }
                }
            }
        }
    }

    Ok(Json(GitBlameResponse {
        line: query.line,
        author,
        time_ago,
        summary,
        hash,
    }))
}

pub async fn git_history(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<GitFileQueryParams>,
) -> Result<Json<GitHistoryResponse>, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, "local", &query.path, false)?;
    let repo_dir = get_directory_path(&query.path);
    let clean_file = query.file.replace('\\', "/");

    let output = tokio::process::Command::new("git")
        .args(["log", "-n", "15", "--pretty=format:%h|%an|%cr|%s", "--", &clean_file])
        .current_dir(&repo_dir)
        .output()
        .await;

    let mut commits = Vec::new();
    if let Ok(out) = output {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                let parts: Vec<&str> = line.splitn(4, '|').collect();
                if parts.len() == 4 {
                    commits.push(GitCommitItem {
                        hash: parts[0].to_string(),
                        author: parts[1].to_string(),
                        time_ago: parts[2].to_string(),
                        summary: parts[3].to_string(),
                    });
                }
            }
        }
    }

    Ok(Json(GitHistoryResponse {
        path: query.file,
        commits,
    }))
}

#[derive(Debug, Deserialize)]
pub struct GitCommitPayload {
    pub path: String,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct GitStagePayload {
    pub path: String,
    pub file: String,
}

#[derive(Debug, Deserialize)]
pub struct GitCheckoutPayload {
    pub path: String,
    pub branch: String,
}

#[derive(Debug, Serialize)]
pub struct GitBranchItem {
    pub name: String,
    pub is_current: bool,
    pub is_remote: bool,
}

#[derive(Debug, Serialize)]
pub struct GitBranchesResponse {
    pub current: String,
    pub branches: Vec<GitBranchItem>,
}

pub async fn git_commit(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Json(payload): Json<GitCommitPayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, "local", &payload.path, true)?;
    let repo_dir = get_directory_path(&payload.path);

    let add_out = tokio::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(&repo_dir)
        .output()
        .await;

    if let Err(e) = add_out {
        return Err(ApiError(RemoteError::ConnectionFailed(format!("git add failed: {}", e))));
    }

    let commit_out = tokio::process::Command::new("git")
        .args(["commit", "-m", &payload.message])
        .current_dir(&repo_dir)
        .output()
        .await;

    match commit_out {
        Ok(out) if out.status.success() => {
            let msg = String::from_utf8_lossy(&out.stdout).to_string();
            Ok(Json(serde_json::json!({ "success": true, "output": msg })))
        }
        Ok(out) => {
            let err_msg = String::from_utf8_lossy(&out.stderr).to_string();
            Err(ApiError(RemoteError::ConnectionFailed(err_msg)))
        }
        Err(e) => Err(ApiError(RemoteError::ConnectionFailed(e.to_string()))),
    }
}

pub async fn git_stage(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Json(payload): Json<GitStagePayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, "local", &payload.path, true)?;
    let repo_dir = get_directory_path(&payload.path);
    let clean_file = payload.file.replace('\\', "/");

    let out = tokio::process::Command::new("git")
        .args(["add", &clean_file])
        .current_dir(&repo_dir)
        .output()
        .await;

    match out {
        Ok(res) if res.status.success() => Ok(Json(serde_json::json!({ "success": true }))),
        Ok(res) => Err(ApiError(RemoteError::ConnectionFailed(String::from_utf8_lossy(&res.stderr).to_string()))),
        Err(e) => Err(ApiError(RemoteError::ConnectionFailed(e.to_string()))),
    }
}

pub async fn git_unstage(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Json(payload): Json<GitStagePayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, "local", &payload.path, true)?;
    let repo_dir = get_directory_path(&payload.path);
    let clean_file = payload.file.replace('\\', "/");

    let out = tokio::process::Command::new("git")
        .args(["restore", "--staged", &clean_file])
        .current_dir(&repo_dir)
        .output()
        .await;

    match out {
        Ok(res) if res.status.success() => Ok(Json(serde_json::json!({ "success": true }))),
        Ok(res) => Err(ApiError(RemoteError::ConnectionFailed(String::from_utf8_lossy(&res.stderr).to_string()))),
        Err(e) => Err(ApiError(RemoteError::ConnectionFailed(e.to_string()))),
    }
}

pub async fn git_revert(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Json(payload): Json<GitStagePayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, "local", &payload.path, true)?;
    let repo_dir = get_directory_path(&payload.path);
    let clean_file = payload.file.replace('\\', "/");

    let out = tokio::process::Command::new("git")
        .args(["checkout", "--", &clean_file])
        .current_dir(&repo_dir)
        .output()
        .await;

    match out {
        Ok(res) if res.status.success() => Ok(Json(serde_json::json!({ "success": true }))),
        Ok(res) => Err(ApiError(RemoteError::ConnectionFailed(String::from_utf8_lossy(&res.stderr).to_string()))),
        Err(e) => Err(ApiError(RemoteError::ConnectionFailed(e.to_string()))),
    }
}

pub async fn git_branches(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<GitQueryParams>,
) -> Result<Json<GitBranchesResponse>, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, "local", &query.path, false)?;
    let repo_dir = get_directory_path(&query.path);

    let out = tokio::process::Command::new("git")
        .args(["branch", "-a"])
        .current_dir(&repo_dir)
        .output()
        .await;

    let mut current = "HEAD".to_string();
    let mut branches = Vec::new();

    if let Ok(res) = out {
        if res.status.success() {
            let text = String::from_utf8_lossy(&res.stdout);
            for line in text.lines() {
                let trimmed = line.trim();
                let is_current = trimmed.starts_with('*');
                let name = trimmed.trim_start_matches('*').trim().to_string();
                let is_remote = name.starts_with("remotes/");
                if is_current {
                    current = name.clone();
                }
                if !name.contains("->") && !name.is_empty() {
                    branches.push(GitBranchItem {
                        name,
                        is_current,
                        is_remote,
                    });
                }
            }
        }
    }

    Ok(Json(GitBranchesResponse { current, branches }))
}

pub async fn git_checkout(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Json(payload): Json<GitCheckoutPayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, "local", &payload.path, true)?;
    let repo_dir = get_directory_path(&payload.path);

    let out = tokio::process::Command::new("git")
        .args(["checkout", &payload.branch])
        .current_dir(&repo_dir)
        .output()
        .await;

    match out {
        Ok(res) if res.status.success() => Ok(Json(serde_json::json!({ "success": true }))),
        Ok(res) => Err(ApiError(RemoteError::ConnectionFailed(String::from_utf8_lossy(&res.stderr).to_string()))),
        Err(e) => Err(ApiError(RemoteError::ConnectionFailed(e.to_string()))),
    }
}

#[derive(Debug, Serialize)]
pub struct ZipEntryItem {
    pub name: String,
    pub uncompressed_size: u64,
    pub compressed_size: u64,
    pub is_dir: bool,
}

#[derive(Debug, Serialize)]
pub struct ZipInspectResponse {
    pub path: String,
    pub entries: Vec<ZipEntryItem>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ZipExtractQuery {
    pub remote: String,
    pub path: String,
    pub entry: String,
}

pub async fn zip_inspect(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<FileQuery>,
) -> Result<Json<ZipInspectResponse>, ApiError> {
    let user = state.current_user(&cookies)?;
    // Reads directly off the server's local disk regardless of `query.remote`.
    authorize(&user, "local", &query.path, false)?;
    let clean = clean_path_str(&query.path);
    let p = std::path::Path::new(&clean);

    let file = std::fs::File::open(p).map_err(|e| ApiError(RemoteError::NotFound(e.to_string())))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| ApiError(RemoteError::ConnectionFailed(e.to_string())))?;

    let mut entries = Vec::new();
    for i in 0..archive.len() {
        if let Ok(entry) = archive.by_index(i) {
            entries.push(ZipEntryItem {
                name: entry.name().to_string(),
                uncompressed_size: entry.size(),
                compressed_size: entry.compressed_size(),
                is_dir: entry.is_dir(),
            });
        }
    }

    Ok(Json(ZipInspectResponse {
        path: query.path,
        entries,
    }))
}

pub async fn zip_extract_entry(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Query(query): Query<ZipExtractQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let user = state.current_user(&cookies)?;
    authorize(&user, "local", &query.path, false)?;
    let clean = clean_path_str(&query.path);
    let p = std::path::Path::new(&clean);

    let file = std::fs::File::open(p).map_err(|e| ApiError(RemoteError::NotFound(e.to_string())))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| ApiError(RemoteError::ConnectionFailed(e.to_string())))?;

    let mut entry = archive.by_name(&query.entry).map_err(|e| ApiError(RemoteError::NotFound(e.to_string())))?;
    let mut buffer = Vec::new();
    std::io::Read::read_to_end(&mut entry, &mut buffer).map_err(|e| ApiError(RemoteError::ConnectionFailed(e.to_string())))?;

    let mime = mime_guess::from_path(&query.entry).first_or_octet_stream().to_string();
    let mut headers = HeaderMap::new();
    headers.insert("content-type", mime.parse().unwrap_or_else(|_| "application/octet-stream".parse().unwrap()));

    Ok((headers, Bytes::from(buffer)))
}
