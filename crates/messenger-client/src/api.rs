//! Typed client of the messenger HTTP API (`proto/turbo_pancake/v1/api.proto`).

use std::time::Duration;

use prost::Message;
use reqwest::{Method, StatusCode};
use turbo_pancake_protocol::{API_CONTENT_TYPE, v1};

#[derive(Debug, Clone, thiserror::Error)]
pub enum ApiError {
    /// The server answered with an error.
    #[error("{message}")]
    Server {
        status: u16,
        code: v1::ApiErrorCode,
        message: String,
    },
    /// No answer (server down, network, timeout): the operation may be retried.
    #[error("server unreachable: {0}")]
    Network(String),
}

impl ApiError {
    pub fn code(&self) -> Option<v1::ApiErrorCode> {
        match self {
            ApiError::Server { code, .. } => Some(*code),
            ApiError::Network(_) => None,
        }
    }

    /// The session is gone (logout elsewhere, revoked, expired).
    pub fn is_unauthenticated(&self) -> bool {
        self.code() == Some(v1::ApiErrorCode::Unauthenticated)
    }

    /// Worth retrying later without user action.
    pub fn is_transient(&self) -> bool {
        match self {
            ApiError::Network(_) => true,
            ApiError::Server { code, .. } => matches!(
                code,
                v1::ApiErrorCode::RateLimited
                    | v1::ApiErrorCode::Unavailable
                    | v1::ApiErrorCode::Internal
            ),
        }
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Clone)]
pub struct Api {
    http: reqwest::Client,
    /// `http://host:port` (no trailing slash).
    base: String,
    token: Option<String>,
}

/// Trust for `https://` servers: always verified (ADR 0014).
pub type TlsConfig = std::sync::Arc<turbo_pancake_tls::rustls::ClientConfig>;

/// The public web PKI (Mozilla roots): a server with a publicly trusted certificate.
pub fn web_pki() -> TlsConfig {
    turbo_pancake_tls::client_config(None).expect("built-in roots")
}

impl Api {
    /// `https://` servers are checked against the public web PKI.
    pub fn new(base: &str) -> Self {
        Self::with_tls(base, web_pki())
    }

    /// `https://` servers are checked against `tls` (e.g. only the server's private CA).
    pub fn with_tls(base: &str, tls: TlsConfig) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .use_preconfigured_tls((*tls).clone())
            .build()
            .expect("HTTP client");
        Self {
            http,
            base: base.trim_end_matches('/').to_owned(),
            token: None,
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn set_token(&mut self, token: Option<String>) {
        self.token = token;
    }

    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    async fn call<R: Message + Default>(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> ApiResult<(StatusCode, R)> {
        let mut req = self.http.request(method, format!("{}{path}", self.base));
        if let Some(t) = &self.token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        if let Some(b) = body {
            req = req.header("content-type", API_CONTENT_TYPE).body(b);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ApiError::Network(without_url(e)))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ApiError::Network(without_url(e)))?;
        if status.is_success() {
            R::decode(bytes)
                .map(|r| (status, r))
                .map_err(|_| ApiError::Network("malformed server answer".into()))
        } else {
            let e = v1::ApiError::decode(bytes).unwrap_or_default();
            Err(ApiError::Server {
                status: status.as_u16(),
                code: v1::ApiErrorCode::try_from(e.code).unwrap_or(v1::ApiErrorCode::Unspecified),
                message: if e.message.is_empty() {
                    format!("server error {status}")
                } else {
                    e.message
                },
            })
        }
    }

    async fn get<R: Message + Default>(&self, path: &str) -> ApiResult<R> {
        self.call(Method::GET, path, None).await.map(|r| r.1)
    }

    async fn send<R: Message + Default>(
        &self,
        method: Method,
        path: &str,
        body: &impl Message,
    ) -> ApiResult<R> {
        self.call(method, path, Some(body.encode_to_vec()))
            .await
            .map(|r| r.1)
    }

    async fn bare<R: Message + Default>(&self, method: Method, path: &str) -> ApiResult<R> {
        self.call(method, path, None).await.map(|r| r.1)
    }

    pub async fn register(&self, req: &v1::RegisterRequest) -> ApiResult<v1::AuthResponse> {
        self.send(Method::POST, "/v1/auth/register", req).await
    }

    pub async fn login(&self, req: &v1::LoginRequest) -> ApiResult<v1::AuthResponse> {
        self.send(Method::POST, "/v1/auth/login", req).await
    }

    pub async fn logout(&self) -> ApiResult<v1::Empty> {
        self.bare(Method::POST, "/v1/auth/logout").await
    }

    pub async fn me(&self) -> ApiResult<v1::User> {
        self.get("/v1/me").await
    }

    pub async fn change_password(
        &self,
        req: &v1::ChangePasswordRequest,
    ) -> ApiResult<v1::AuthResponse> {
        self.send(Method::POST, "/v1/me/password", req).await
    }

    pub async fn sessions(&self) -> ApiResult<v1::SessionList> {
        self.get("/v1/sessions").await
    }

    pub async fn revoke_session(&self, id: &str) -> ApiResult<v1::Empty> {
        self.bare(Method::DELETE, &format!("/v1/sessions/{id}"))
            .await
    }

    /// `username` must already be valid (`[A-Za-z0-9_]`), so it needs no escaping.
    pub async fn lookup(&self, username: &str) -> ApiResult<v1::User> {
        self.get(&format!("/v1/users/lookup?username={username}"))
            .await
    }

    pub async fn block(&self, user_id: &str) -> ApiResult<v1::Empty> {
        self.bare(Method::PUT, &format!("/v1/blocks/{user_id}"))
            .await
    }

    pub async fn unblock(&self, user_id: &str) -> ApiResult<v1::Empty> {
        self.bare(Method::DELETE, &format!("/v1/blocks/{user_id}"))
            .await
    }

    pub async fn conversations(&self) -> ApiResult<v1::ConversationList> {
        self.get("/v1/conversations").await
    }

    pub async fn create_direct(&self, user_id: &str) -> ApiResult<v1::Conversation> {
        self.send(
            Method::POST,
            "/v1/conversations/direct",
            &v1::CreateDirectRequest {
                user_id: user_id.into(),
            },
        )
        .await
    }

    pub async fn changes(&self, conv: &str, after: i64, limit: u32) -> ApiResult<v1::MessagePage> {
        self.get(&format!(
            "/v1/conversations/{conv}/changes?after={after}&limit={limit}"
        ))
        .await
    }

    pub async fn send_message(
        &self,
        conv: &str,
        req: &v1::SendMessageRequest,
    ) -> ApiResult<v1::ChatMessage> {
        self.send(
            Method::POST,
            &format!("/v1/conversations/{conv}/messages"),
            req,
        )
        .await
    }

    pub async fn edit_message(
        &self,
        conv: &str,
        msg: &str,
        body: &str,
    ) -> ApiResult<v1::ChatMessage> {
        self.send(
            Method::PATCH,
            &format!("/v1/conversations/{conv}/messages/{msg}"),
            &v1::EditMessageRequest { body: body.into() },
        )
        .await
    }

    pub async fn delete_message(&self, conv: &str, msg: &str) -> ApiResult<v1::ChatMessage> {
        self.bare(
            Method::DELETE,
            &format!("/v1/conversations/{conv}/messages/{msg}"),
        )
        .await
    }

    pub async fn mark_read(&self, conv: &str, delivered: i64, read: i64) -> ApiResult<v1::Receipt> {
        self.send(
            Method::POST,
            &format!("/v1/conversations/{conv}/read"),
            &v1::MarkReadRequest {
                delivered_sequence: delivered,
                read_sequence: read,
            },
        )
        .await
    }
}

/// Shown when the server's certificate is not trusted (wrong or missing `--server-ca`,
/// expired certificate, address not in the certificate). Never silently ignored.
pub const UNTRUSTED_CERTIFICATE: &str = "server certificate is not trusted";

/// Short reason of a connection error; a certificate problem anywhere in the chain of
/// causes becomes [`UNTRUSTED_CERTIFICATE`].
pub fn describe(e: &(dyn std::error::Error + 'static)) -> String {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        let text = err.to_string().to_ascii_lowercase();
        if text.contains("certificate") || text.contains("unknownissuer") {
            return UNTRUSTED_CERTIFICATE.into();
        }
        cur = err.source();
    }
    "cannot connect".into()
}

/// reqwest errors print the URL; keep logs and UI free of query strings.
fn without_url(e: reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out".into()
    } else if e.is_connect() {
        describe(&e)
    } else {
        e.without_url().to_string()
    }
}
