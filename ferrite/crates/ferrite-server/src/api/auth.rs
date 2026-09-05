//! Sessions, the current user's settings, and admin management of users and
//! remotes.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use tower_cookies::{Cookie, Cookies};

use super::{
    build_backend, check_password_policy, invalid, require_admin, role_str, ApiError, AppState,
    UserSettings, SESSION_COOKIE,
};
use crate::auth::{hash_password, verify_password, verify_plain, verify_totp};
use ferrite_core::{PathGrant, ProtocolType, RemoteError, Role};

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

fn session_cookie(state: &AppState, value: String) -> Cookie<'static> {
    let mut cookie = Cookie::new(SESSION_COOKIE, value);
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_secure(state.secure_cookies);
    cookie.set_same_site(tower_cookies::cookie::SameSite::Lax);
    cookie
}

pub async fn login_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    cookies: Cookies,
    Json(payload): Json<LoginPayload>,
) -> Result<impl IntoResponse, ApiError> {
    let client_key = state.client_key(&headers, peer);
    state.check_rate_limit(&client_key)?;

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
            tracing::warn!("Login failed from {}: unknown or disabled username '{}'", client_key, payload.username);
            state.record_failed_attempt(&client_key);
            return Err(ApiError(RemoteError::AuthFailed("Invalid credentials".into())));
        }
    };

    let hashed = user.password_hash.starts_with("$argon2");
    let password_valid = if hashed {
        verify_password(user.password_hash.trim(), payload.password.trim())
    } else {
        // Legacy: a plaintext password_hash. Still accepted so nobody is
        // locked out by an upgrade, but compared in constant time and called
        // out loudly, because the config file is then itself a credential.
        verify_plain(user.password_hash.trim(), payload.password.trim())
    };
    if password_valid && !hashed {
        tracing::warn!(
            "User '{}' has a plaintext password_hash in the config. Replace it with the output of `ferrite hash-password`.",
            user.username
        );
    }

    if !password_valid {
        tracing::warn!("Login failed from {}: password verification failed for '{}'", client_key, payload.username);
        state.record_failed_attempt(&client_key);
        return Err(ApiError(RemoteError::AuthFailed("Invalid credentials".into())));
    }

    if let Some(secret) = user.totp_secret.as_deref().filter(|s| !s.trim().is_empty()) {
        let code = payload.totp_code.as_deref().unwrap_or_default();
        if !verify_totp(secret, code) {
            state.record_failed_attempt(&client_key);
            return Err(ApiError(RemoteError::AuthFailed("Invalid TOTP code".into())));
        }
    }

    state.reset_rate_limit(&client_key);
    let token = state.create_session(&user.username);
    cookies.add(session_cookie(&state, token));
    tracing::info!("User '{}' logged in from {}", user.username, client_key);

    Ok((StatusCode::OK, Json(serde_json::json!({ "status": "authenticated", "role": role_str(user.role) }))))
}

pub async fn logout_handler(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
) -> Result<impl IntoResponse, ApiError> {
    if let Some(cookie) = cookies.get(SESSION_COOKIE) {
        state.end_session(cookie.value());
    }
    let mut cookie = session_cookie(&state, String::new());
    cookie.set_max_age(tower_cookies::cookie::time::Duration::ZERO);
    cookies.remove(cookie);
    Ok(StatusCode::OK)
}

pub async fn get_settings(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
) -> Result<impl IntoResponse, ApiError> {
    state.current_user(&cookies)?;
    let settings = state.settings.lock().unwrap();
    Ok(Json(settings.clone()))
}

pub async fn save_settings(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Json(payload): Json<UserSettings>,
) -> Result<impl IntoResponse, ApiError> {
    state.current_user(&cookies)?;
    let color = payload.accent_color.trim();
    let is_hex_color = color.len() == 7
        && color.starts_with('#')
        && color[1..].bytes().all(|b| b.is_ascii_hexdigit());
    if !is_hex_color {
        return Err(invalid("accent_color must be a #rrggbb value"));
    }
    *state.settings.lock().unwrap() = UserSettings { accent_color: color.to_string() };
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
    check_password_policy(&payload.new_password)?;

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
            verify_plain(&user.password_hash, provided)
        };
        if !valid {
            return Err(ApiError(RemoteError::AuthFailed("Current password is incorrect".into())));
        }

        user.password_hash = hash_password(&payload.new_password)
            .map_err(|e| RemoteError::OperationFailed(format!("Failed to hash password: {}", e)))?;
    }
    state.persist_config();

    Ok(StatusCode::OK)
}

#[derive(Debug, Serialize)]
pub struct RemoteSummary {
    pub name: String,
    pub protocol: String,
    pub host: String,
    pub port: u16,
    pub username: String,
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
        .filter(|r| user.can_use_remote(&r.name))
        .map(|r| RemoteSummary {
            name: r.name.clone(),
            protocol: format!("{:?}", r.protocol).to_lowercase(),
            host: r.host.clone(),
            port: r.port,
            username: r.username.clone(),
        })
        .collect();

    if user.can_use_remote("local") && !summaries.iter().any(|r| r.name == "local" || r.protocol == "local") {
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
    // Not all of these have a form field. An omitted one means "leave as
    // is", never "clear": see `upsert_remote`.
    pub agent_pubkey: Option<String>,
    pub base_path: Option<String>,
    pub endpoint: Option<String>,
    pub use_ssl: Option<bool>,
}

fn parse_protocol(s: &str) -> Result<ProtocolType, ApiError> {
    Ok(match s.to_lowercase().as_str() {
        "sftp" => ProtocolType::Sftp,
        "ftp" => ProtocolType::Ftp,
        "ftps" => ProtocolType::Ftps,
        "webdav" => ProtocolType::Webdav,
        "s3" => ProtocolType::S3,
        "local" => ProtocolType::Local,
        "ferrous" => ProtocolType::Ferrous,
        _ => return Err(invalid("Invalid protocol")),
    })
}

/// Remote names are used as map keys, in URLs and in grants, so keep them
/// to something that survives all three unchanged.
fn check_remote_name(name: &str) -> Result<(), ApiError> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(invalid("Remote name must be 1-64 characters of letters, digits, '-', '_' or '.'"))
    }
}

pub async fn upsert_remote(
    State(state): State<Arc<AppState>>,
    cookies: Cookies,
    Json(payload): Json<UpsertRemotePayload>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &cookies)?;
    let name = payload.name.trim().to_string();
    check_remote_name(&name)?;
    let protocol = parse_protocol(&payload.protocol)?;

    // Saving must not destroy settings the form cannot express: writing null
    // for an absent agent_pubkey would silently turn off pinning.
    let existing = {
        let config = state.config.lock().unwrap();
        config.remotes.iter().find(|r| r.name == name).cloned()
    };
    let previous = existing.as_ref();

    let remote_config = ferrite_core::RemoteConfig {
        name: name.clone(),
        protocol,
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

    // A bad pin is rejected here rather than stored and failed open later.
    let backend = build_backend(&remote_config, &state.client_keypair).map_err(ApiError)?;

    state.backends.lock().unwrap().insert(name.clone(), backend);
    {
        let mut config = state.config.lock().unwrap();
        if let Some(pos) = config.remotes.iter().position(|r| r.name == name) {
            config.remotes[pos] = remote_config;
        } else {
            config.remotes.push(remote_config);
        }
    }
    state.persist_config();

    Ok(StatusCode::OK)
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
        _ => Err(invalid("Invalid role")),
    }
}

fn check_username(name: &str) -> Result<(), ApiError> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'));
    if ok {
        Ok(())
    } else {
        Err(invalid("Username must be 1-64 characters of letters, digits, '-', '_', '.' or '@'"))
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
    check_username(&username)?;
    check_password_policy(&payload.password)?;
    let role = parse_role(&payload.role)?;
    let password_hash = hash_password(&payload.password)
        .map_err(|e| RemoteError::OperationFailed(format!("Failed to hash password: {}", e)))?;

    {
        let mut config = state.config.lock().unwrap();
        if config.users.iter().any(|u| u.username == username) {
            return Err(invalid("Username already exists"));
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
    let mut disabled = false;

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
                return Err(invalid("Cannot demote the last admin"));
            }
            user.role = new_role;
        }
        if let Some(enabled) = payload.enabled {
            if !enabled && user.username == admin.username {
                return Err(invalid("Cannot disable your own account"));
            }
            if !enabled && user.role == Role::Admin && is_last_admin {
                return Err(invalid("Cannot disable the last admin"));
            }
            user.enabled = enabled;
            disabled = !enabled;
        }
        if let Some(allow_shell) = payload.allow_shell {
            user.allow_shell = allow_shell;
        }
        if let Some(permissions) = payload.permissions {
            user.permissions = permissions;
        }
        if let Some(password) = payload.password.filter(|p| !p.trim().is_empty()) {
            check_password_policy(&password)?;
            user.password_hash = hash_password(&password)
                .map_err(|e| RemoteError::OperationFailed(format!("Failed to hash password: {}", e)))?;
        }
    }
    if disabled {
        state.end_sessions_for(&username);
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
        return Err(invalid("Cannot delete your own account"));
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
            return Err(invalid("Cannot delete the last admin"));
        }
        config.users.retain(|u| u.username != username);
    }
    state.end_sessions_for(&username);
    state.persist_config();

    Ok(StatusCode::OK)
}
