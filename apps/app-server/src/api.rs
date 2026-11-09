//! HTTP plumbing of the messenger API (ADR 0013): protobuf bodies, `ApiError` replies,
//! the authenticated-user extractor and the database handle.

use std::net::{IpAddr, SocketAddr};

use axum::body::Bytes;
use axum::extract::{ConnectInfo, FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use prost::Message;
use turbo_pancake_protocol::{self as proto, ValidationError, v1};
use uuid::Uuid;

use crate::AppState;
use crate::auth;

pub type Code = v1::ApiErrorCode;

/// An error reply: HTTP status + `ApiError` body. The message never holds secrets or
/// message content.
#[derive(Debug)]
pub struct ApiError {
    pub code: Code,
    pub message: String,
}

impl ApiError {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(Code::BadRequest, message)
    }
    pub fn not_found() -> Self {
        Self::new(Code::NotFound, "not found")
    }
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(Code::Forbidden, message)
    }
    pub fn unauthenticated() -> Self {
        Self::new(Code::Unauthenticated, "not signed in or session ended")
    }
    pub fn rate_limited() -> Self {
        Self::new(Code::RateLimited, "too many requests, try again later")
    }
    pub fn internal() -> Self {
        Self::new(Code::Internal, "internal error")
    }

    fn status(&self) -> StatusCode {
        match self.code {
            Code::BadRequest | Code::Unspecified => StatusCode::BAD_REQUEST,
            Code::Unauthenticated => StatusCode::UNAUTHORIZED,
            Code::Forbidden => StatusCode::FORBIDDEN,
            Code::NotFound => StatusCode::NOT_FOUND,
            Code::Conflict => StatusCode::CONFLICT,
            Code::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Code::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Code::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Code::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl From<ValidationError> for ApiError {
    fn from(e: ValidationError) -> Self {
        Self::bad_request(e.to_string())
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        // sqlx errors carry no bind values; still, log only the kind of failure.
        match db_unavailable(&e) {
            Some(why) => {
                tracing::warn!(error = %e, why, "database unavailable");
                Self::new(Code::Unavailable, why)
            }
            None => {
                tracing::error!(error = %e, "database error");
                Self::internal()
            }
        }
    }
}

/// Failures that are the database's state, not the request (S8 step 4): the client gets
/// `503` + `Retry-After` and may retry; everything else stays `500`.
pub fn db_unavailable(e: &sqlx::Error) -> Option<&'static str> {
    match e {
        // Pool exhausted for `acquire_timeout`, or connections cannot be opened.
        sqlx::Error::PoolTimedOut => Some("database busy, try again"),
        sqlx::Error::PoolClosed | sqlx::Error::Io(_) | sqlx::Error::Tls(_) => {
            Some("database unavailable, try again")
        }
        sqlx::Error::Database(d) => {
            let code = d.code()?;
            // Class 53: insufficient resources (53100 disk_full, 53200 out_of_memory,
            // 53300 too_many_connections, 53400 configuration_limit_exceeded).
            // Class 57P: the server is shutting down / starting / crashed.
            // 08: connection exceptions.
            if code.starts_with("53") {
                Some("database storage or resources exhausted, try again later")
            } else if code.starts_with("57P") || code.starts_with("08") {
                Some("database unavailable, try again")
            } else {
                None
            }
        }
        _ => None,
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let retry = matches!(self.code, Code::Unavailable);
        let body = v1::ApiError {
            code: self.code as i32,
            message: self.message,
        };
        let mut resp = (status, Proto(body)).into_response();
        if retry {
            resp.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("2"));
        }
        resp
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

/// A protobuf request or response body.
pub struct Proto<T>(pub T);

impl<T: Message> IntoResponse for Proto<T> {
    fn into_response(self) -> Response {
        (
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static(proto::API_CONTENT_TYPE),
            )],
            self.0.encode_to_vec(),
        )
            .into_response()
    }
}

impl<S: Send + Sync, T: Message + Default> FromRequest<S> for Proto<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, _state: &S) -> Result<Self, Self::Rejection> {
        let content_type = req
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if content_type != proto::API_CONTENT_TYPE {
            return Err(ApiError::bad_request(format!(
                "content type must be {}",
                proto::API_CONTENT_TYPE
            )));
        }
        let bytes: Bytes = axum::body::to_bytes(req.into_body(), proto::MAX_API_BODY_BYTES)
            .await
            .map_err(|_| ApiError::new(Code::PayloadTooLarge, "request body too large"))?;
        T::decode(bytes)
            .map(Proto)
            .map_err(|_| ApiError::bad_request("malformed protobuf body"))
    }
}

/// The messenger database; `503` when the server runs without one.
#[derive(Clone)]
pub struct Db(pub sqlx::PgPool);

impl FromRequestParts<AppState> for Db {
    type Rejection = ApiError;

    async fn from_request_parts(_: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        state
            .db
            .clone()
            .map(Db)
            .ok_or_else(|| ApiError::new(Code::Unavailable, "messenger is not enabled"))
    }
}

/// The caller's address (rate limits, security log).
pub struct ClientIp(pub IpAddr);

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, ApiError> {
        parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ci| ClientIp(ci.0.ip()))
            .ok_or_else(ApiError::internal)
    }
}

/// A request with a valid `Authorization: Bearer <token>` of an active session.
#[derive(Clone, Debug)]
pub struct AuthUser {
    pub user_id: Uuid,
    pub session_id: Uuid,
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let Db(pool) = Db::from_request_parts(parts, state).await?;
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .filter(|t| !t.is_empty() && t.len() <= proto::MAX_TOKEN_LEN)
            .ok_or_else(ApiError::unauthenticated)?;
        auth::authenticate(&pool, token)
            .await?
            .ok_or_else(ApiError::unauthenticated)
    }
}

/// Parses a path id (UUID) or answers 404 (an invalid id names no object).
pub fn parse_id(id: &str) -> ApiResult<Uuid> {
    proto::validate_id(id).map_err(|_| ApiError::not_found())?;
    Uuid::parse_str(id).map_err(|_| ApiError::not_found())
}

/// Unix milliseconds of a database timestamp.
pub fn ms(t: time::OffsetDateTime) -> i64 {
    (t.unix_timestamp_nanos() / 1_000_000) as i64
}
