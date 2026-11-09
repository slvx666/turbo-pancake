//! Accounts and sessions (spec §8, ADR 0013 §5–6): Argon2id passwords, opaque session
//! tokens stored as SHA-256, sliding expiry, rotation on refresh, revocation.
//!
//! Security log: target `security`, ids and addresses only — never passwords, tokens
//! or attempted usernames (a password typed into the username field must not leak).

use std::sync::OnceLock;
use std::time::Duration;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use base64::Engine as _;
use sqlx::PgPool;
use turbo_pancake_protocol::{self as proto, v1};
use uuid::Uuid;

use crate::AppState;
use crate::api::{ApiError, ApiResult, AuthUser, ClientIp, Code, Db, Proto, ms, parse_id};
use crate::hashing::Hasher;

/// A session unused for this long ends (every use extends it).
pub const SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 3600);
/// `last_seen_at` / expiry are written at most this often per session.
const TOUCH_EVERY_SECS: i64 = 60;

fn argon2() -> Argon2<'static> {
    // RFC 9106 / OWASP: Argon2id, 19 MiB, 2 passes, 1 lane.
    let params = Params::new(19 * 1024, 2, 1, None).expect("argon2 params");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

fn salt() -> SaltString {
    let mut bytes = [0u8; 16];
    random(&mut bytes);
    SaltString::encode_b64(&bytes).expect("salt")
}

fn random(buf: &mut [u8]) {
    aws_lc_rs::rand::fill(buf).expect("system random generator");
}

/// Argon2id PHC string; runs on the blocking pool (≈ 20–40 ms of CPU) within the
/// hashing bound (`503` when the server is saturated).
pub async fn hash_password(hasher: &Hasher, password: String) -> ApiResult<String> {
    hasher
        .run(move || {
            argon2()
                .hash_password(password.as_bytes(), &salt())
                .map(|h| h.to_string())
        })
        .await?
        .map_err(|_| ApiError::internal())
}

/// Hash verified when the user does not exist, so both failures cost the same.
fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| {
        argon2()
            .hash_password(b"not a real password", &salt())
            .expect("dummy hash")
            .to_string()
    })
}

/// `Ok(false)` = wrong password or unknown user (same cost); `Err` = the server could not
/// check now (saturated): never reported as a wrong password.
pub async fn verify_password(
    hasher: &Hasher,
    hash: Option<String>,
    password: String,
) -> ApiResult<bool> {
    hasher
        .run(move || {
            let stored = hash.as_deref().unwrap_or_else(|| dummy_hash());
            let ok = PasswordHash::new(stored)
                .map(|h| argon2().verify_password(password.as_bytes(), &h).is_ok())
                .unwrap_or(false);
            ok && hash.is_some()
        })
        .await
}

fn token_hash(token: &str) -> Vec<u8> {
    aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, token.as_bytes())
        .as_ref()
        .to_vec()
}

fn new_token() -> String {
    let mut bytes = [0u8; 32];
    random(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The active session of `token`, extending its expiry at most once a minute.
pub async fn authenticate(pool: &PgPool, token: &str) -> ApiResult<Option<AuthUser>> {
    let row: Option<(Uuid, Uuid, bool)> = sqlx::query_as(
        "SELECT s.id, s.user_id, s.last_seen_at < now() - make_interval(secs => $2)
           FROM sessions s JOIN users u ON u.id = s.user_id
          WHERE s.token_hash = $1 AND s.revoked_at IS NULL AND s.expires_at > now()
            AND u.status = 'active'",
    )
    .bind(token_hash(token))
    .bind(TOUCH_EVERY_SECS as f64)
    .fetch_optional(pool)
    .await?;
    let Some((session_id, user_id, stale)) = row else {
        return Ok(None);
    };
    if stale {
        sqlx::query(
            "UPDATE sessions SET last_seen_at = now(), expires_at = now() + make_interval(secs => $2)
              WHERE id = $1",
        )
        .bind(session_id)
        .bind(SESSION_TTL.as_secs() as f64)
        .execute(pool)
        .await?;
    }
    Ok(Some(AuthUser {
        user_id,
        session_id,
    }))
}

async fn create_session(
    pool: &PgPool,
    user_id: Uuid,
    device: &str,
) -> ApiResult<(String, Uuid, i64)> {
    let token = new_token();
    let id = Uuid::new_v4();
    let expires: time::OffsetDateTime = sqlx::query_scalar(
        "INSERT INTO sessions (id, user_id, token_hash, device_id, expires_at)
         VALUES ($1, $2, $3, $4, now() + make_interval(secs => $5)) RETURNING expires_at",
    )
    .bind(id)
    .bind(user_id)
    .bind(token_hash(&token))
    .bind(device)
    .bind(SESSION_TTL.as_secs() as f64)
    .fetch_one(pool)
    .await?;
    Ok((token, id, ms(expires)))
}

#[derive(sqlx::FromRow)]
pub struct UserRow {
    pub id: Uuid,
    pub username_display: String,
    pub display_name: String,
}

impl UserRow {
    pub fn to_proto(&self, online: bool) -> v1::User {
        v1::User {
            user_id: self.id.to_string(),
            username: self.username_display.clone(),
            display_name: self.display_name.clone(),
            online,
        }
    }
}

pub async fn user_by_id(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<UserRow>> {
    sqlx::query_as(
        "SELECT id, username_display, display_name FROM users WHERE id = $1 AND status = 'active'",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

async fn auth_response(pool: &PgPool, user_id: Uuid, device: &str) -> ApiResult<v1::AuthResponse> {
    let (token, session_id, expires_at_ms) = create_session(pool, user_id, device).await?;
    let user = user_by_id(pool, user_id)
        .await?
        .ok_or_else(ApiError::internal)?;
    Ok(v1::AuthResponse {
        token,
        session_id: session_id.to_string(),
        user: Some(user.to_proto(true)),
        expires_at_ms,
    })
}

pub async fn register(
    State(state): State<AppState>,
    Db(pool): Db,
    ClientIp(ip): ClientIp,
    Proto(req): Proto<v1::RegisterRequest>,
) -> ApiResult<(StatusCode, Proto<v1::AuthResponse>)> {
    if !state.limiters.register.check(&ip) {
        tracing::warn!(target: "security", event = "register_rate_limited", %ip);
        return Err(ApiError::rate_limited());
    }
    proto::validate_username(&req.username)?;
    proto::validate_password(&req.password)?;
    proto::validate_device_name(&req.device_name)?;
    let display = if req.display_name.is_empty() {
        req.username.clone()
    } else {
        req.display_name.clone()
    };
    proto::validate_display_name(&display)?;
    let hash = hash_password(&state.hasher, req.password).await?;
    let id = Uuid::new_v4();
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO users (id, username_key, username_display, display_name, password_hash)
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT (username_key) DO NOTHING RETURNING id",
    )
    .bind(id)
    .bind(proto::username_key(&req.username))
    .bind(&req.username)
    .bind(&display)
    .bind(&hash)
    .fetch_optional(&pool)
    .await?;
    if inserted.is_none() {
        return Err(ApiError::new(Code::Conflict, "username is taken"));
    }
    tracing::info!(target: "security", event = "registered", user = %id, %ip);
    let resp = auth_response(&pool, id, &req.device_name).await?;
    Ok((StatusCode::CREATED, Proto(resp)))
}

pub async fn login(
    State(state): State<AppState>,
    Db(pool): Db,
    ClientIp(ip): ClientIp,
    Proto(req): Proto<v1::LoginRequest>,
) -> ApiResult<Proto<v1::AuthResponse>> {
    if !state.limiters.login.check(&ip) {
        tracing::warn!(target: "security", event = "login_rate_limited", %ip);
        return Err(ApiError::rate_limited());
    }
    proto::validate_device_name(&req.device_name)?;
    if req.username.len() > proto::MAX_USERNAME_LEN
        || req.password.chars().count() > proto::MAX_PASSWORD_CHARS
    {
        return Err(ApiError::new(
            Code::Unauthenticated,
            "wrong username or password",
        ));
    }
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT id, password_hash FROM users WHERE username_key = $1 AND status = 'active'",
    )
    .bind(proto::username_key(&req.username))
    .fetch_optional(&pool)
    .await?;
    let (id, hash) = row.unzip();
    if !verify_password(&state.hasher, hash, req.password).await? {
        tracing::info!(target: "security", event = "login_failed", %ip);
        return Err(ApiError::new(
            Code::Unauthenticated,
            "wrong username or password",
        ));
    }
    let id = id.expect("verified user has an id");
    tracing::info!(target: "security", event = "login", user = %id, %ip);
    Ok(Proto(auth_response(&pool, id, &req.device_name).await?))
}

pub async fn refresh(Db(pool): Db, me: AuthUser) -> ApiResult<Proto<v1::AuthResponse>> {
    let token = new_token();
    let expires: Option<time::OffsetDateTime> = sqlx::query_scalar(
        "UPDATE sessions SET token_hash = $2, last_seen_at = now(),
                expires_at = now() + make_interval(secs => $3)
          WHERE id = $1 AND revoked_at IS NULL RETURNING expires_at",
    )
    .bind(me.session_id)
    .bind(token_hash(&token))
    .bind(SESSION_TTL.as_secs() as f64)
    .fetch_optional(&pool)
    .await?;
    let expires = expires.ok_or_else(ApiError::unauthenticated)?;
    let user = user_by_id(&pool, me.user_id)
        .await?
        .ok_or_else(ApiError::unauthenticated)?;
    Ok(Proto(v1::AuthResponse {
        token,
        session_id: me.session_id.to_string(),
        user: Some(user.to_proto(true)),
        expires_at_ms: ms(expires),
    }))
}

async fn revoke(pool: &PgPool, state: &AppState, user: Uuid, session: Uuid) -> ApiResult<bool> {
    let done = sqlx::query(
        "UPDATE sessions SET revoked_at = now()
          WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
    )
    .bind(session)
    .bind(user)
    .execute(pool)
    .await?
    .rows_affected()
        > 0;
    if done {
        state.registry.revoke_session(session);
        tracing::info!(target: "security", event = "session_revoked", %user, %session);
    }
    Ok(done)
}

pub async fn logout(
    State(state): State<AppState>,
    Db(pool): Db,
    me: AuthUser,
) -> ApiResult<Proto<v1::Empty>> {
    revoke(&pool, &state, me.user_id, me.session_id).await?;
    Ok(Proto(v1::Empty {}))
}

pub async fn me(
    State(state): State<AppState>,
    Db(pool): Db,
    me: AuthUser,
) -> ApiResult<Proto<v1::User>> {
    let user = user_by_id(&pool, me.user_id)
        .await?
        .ok_or_else(ApiError::unauthenticated)?;
    Ok(Proto(user.to_proto(state.registry.is_online(me.user_id))))
}

pub async fn update_me(
    State(state): State<AppState>,
    Db(pool): Db,
    me: AuthUser,
    Proto(req): Proto<v1::UpdateProfileRequest>,
) -> ApiResult<Proto<v1::User>> {
    proto::validate_display_name(&req.display_name)?;
    sqlx::query("UPDATE users SET display_name = $2 WHERE id = $1")
        .bind(me.user_id)
        .bind(&req.display_name)
        .execute(&pool)
        .await?;
    let user = user_by_id(&pool, me.user_id)
        .await?
        .ok_or_else(ApiError::unauthenticated)?;
    crate::chat::notify_contacts_changed(&pool, &state, me.user_id).await?;
    Ok(Proto(user.to_proto(state.registry.is_online(me.user_id))))
}

pub async fn change_password(
    State(state): State<AppState>,
    Db(pool): Db,
    me: AuthUser,
    Proto(req): Proto<v1::ChangePasswordRequest>,
) -> ApiResult<Proto<v1::AuthResponse>> {
    proto::validate_password(&req.new_password)?;
    let hash: Option<String> = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
        .bind(me.user_id)
        .fetch_optional(&pool)
        .await?;
    if req.old_password.chars().count() > proto::MAX_PASSWORD_CHARS
        || !verify_password(&state.hasher, hash, req.old_password).await?
    {
        tracing::info!(target: "security", event = "password_change_failed", user = %me.user_id);
        return Err(ApiError::forbidden("current password is wrong"));
    }
    let new_hash = hash_password(&state.hasher, req.new_password).await?;
    // Every other session ends; this one stays with a new token.
    let token = new_token();
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
        .bind(me.user_id)
        .bind(&new_hash)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE sessions SET revoked_at = now()
          WHERE user_id = $1 AND id <> $2 AND revoked_at IS NULL",
    )
    .bind(me.user_id)
    .bind(me.session_id)
    .execute(&mut *tx)
    .await?;
    let expires: time::OffsetDateTime = sqlx::query_scalar(
        "UPDATE sessions SET token_hash = $2, expires_at = now() + make_interval(secs => $3)
          WHERE id = $1 RETURNING expires_at",
    )
    .bind(me.session_id)
    .bind(token_hash(&token))
    .bind(SESSION_TTL.as_secs() as f64)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    state.registry.revoke_user(me.user_id, Some(me.session_id));
    tracing::info!(target: "security", event = "password_changed", user = %me.user_id);
    let user = user_by_id(&pool, me.user_id)
        .await?
        .ok_or_else(ApiError::unauthenticated)?;
    Ok(Proto(v1::AuthResponse {
        token,
        session_id: me.session_id.to_string(),
        user: Some(user.to_proto(true)),
        expires_at_ms: ms(expires),
    }))
}

pub async fn list_sessions(Db(pool): Db, me: AuthUser) -> ApiResult<Proto<v1::SessionList>> {
    let rows: Vec<(Uuid, String, time::OffsetDateTime, time::OffsetDateTime)> = sqlx::query_as(
        "SELECT id, device_id, created_at, last_seen_at FROM sessions
          WHERE user_id = $1 AND revoked_at IS NULL AND expires_at > now()
          ORDER BY created_at",
    )
    .bind(me.user_id)
    .fetch_all(&pool)
    .await?;
    let sessions = rows
        .into_iter()
        .map(|(id, device, created, seen)| v1::SessionInfo {
            session_id: id.to_string(),
            device_name: device,
            created_at_ms: ms(created),
            last_seen_at_ms: ms(seen),
            current: id == me.session_id,
        })
        .collect();
    Ok(Proto(v1::SessionList { sessions }))
}

pub async fn revoke_session(
    State(state): State<AppState>,
    Db(pool): Db,
    me: AuthUser,
    Path(id): Path<String>,
) -> ApiResult<Proto<v1::Empty>> {
    let id = parse_id(&id)?;
    if !revoke(&pool, &state, me.user_id, id).await? {
        return Err(ApiError::not_found());
    }
    Ok(Proto(v1::Empty {}))
}
