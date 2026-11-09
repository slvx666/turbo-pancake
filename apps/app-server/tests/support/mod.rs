//! Small harness shared by the hardening tests (S8 step 4): a real server over TCP.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::time::Duration;

use prost::Message as _;
use reqwest::{Method, StatusCode};
use turbo_pancake_app_server::Config;
use turbo_pancake_app_server::ratelimit::Limits;
use turbo_pancake_protocol::API_CONTENT_TYPE;
use turbo_pancake_protocol::v1;

pub struct Server {
    pub addr: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    pub task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl Server {
    pub async fn start(db: Option<&str>, tweak: impl FnOnce(&mut Config)) -> Self {
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_env_filter("warn")
            .try_init();
        let mut config = Config {
            database_url: db.map(str::to_owned),
            database_pool: 8,
            limits: Limits::relaxed(),
            media_udp: false,
            ..Default::default()
        };
        tweak(&mut config);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(turbo_pancake_app_server::serve(listener, config, async {
            let _ = rx.await;
        }));
        let s = Self {
            addr,
            stop: Some(tx),
            task: Some(task),
        };
        let api = s.api();
        for _ in 0..400 {
            if api.http.get(api.url("/healthz")).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        s
    }

    pub fn api(&self) -> Api {
        Api::new(self.addr)
    }

    /// Sends the shutdown request; the server task keeps running until it finishes.
    pub fn request_stop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
    }
}

pub struct Reply {
    pub status: StatusCode,
    pub retry_after: Option<String>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn error(&self) -> v1::ApiError {
        v1::ApiError::decode(self.body.as_slice()).unwrap_or_default()
    }
    pub fn decode<T: prost::Message + Default>(&self) -> T {
        assert!(
            self.status.is_success(),
            "HTTP {}: {:?}",
            self.status,
            self.error()
        );
        T::decode(self.body.as_slice()).unwrap()
    }
}

pub struct Api {
    pub http: reqwest::Client,
    pub base: String,
}

impl Api {
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            http: reqwest::Client::builder()
                .use_preconfigured_tls((*turbo_pancake_tls::client_config(None).unwrap()).clone())
                .pool_max_idle_per_host(0)
                .build()
                .unwrap(),
            base: format!("http://{addr}"),
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub async fn raw(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<Vec<u8>>,
    ) -> reqwest::Result<Reply> {
        let mut req = self.http.request(method, self.url(path));
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        if let Some(b) = body {
            req = req.header("content-type", API_CONTENT_TYPE).body(b);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        Ok(Reply {
            status,
            retry_after,
            body: resp.bytes().await?.to_vec(),
        })
    }

    pub async fn text(&self, path: &str) -> (StatusCode, String) {
        let r = self.http.get(self.url(path)).send().await.unwrap();
        (r.status(), r.text().await.unwrap())
    }

    pub async fn stats(&self) -> serde_json::Value {
        let (_, t) = self.text("/v1/server-stats").await;
        serde_json::from_str(&t).unwrap()
    }

    pub async fn register(&self, name: &str) -> v1::AuthResponse {
        let req = v1::RegisterRequest {
            username: name.into(),
            password: "correct horse battery".into(),
            display_name: String::new(),
            device_name: "test".into(),
        };
        self.raw(
            Method::POST,
            "/v1/auth/register",
            None,
            Some(req.encode_to_vec()),
        )
        .await
        .unwrap()
        .decode()
    }

    pub async fn login(&self, name: &str) -> reqwest::Result<Reply> {
        let req = v1::LoginRequest {
            username: name.into(),
            password: "correct horse battery".into(),
            device_name: "test".into(),
        };
        self.raw(
            Method::POST,
            "/v1/auth/login",
            None,
            Some(req.encode_to_vec()),
        )
        .await
    }
}
