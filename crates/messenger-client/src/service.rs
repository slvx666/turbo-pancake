//! The messenger worker thread: owns the sync engine, its own signed-in WebSocket for
//! events, reconnects with backoff, and turns UI commands into API calls. The UI never
//! waits on it: commands go in through a channel, views come back through a callback.

use std::path::PathBuf;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use turbo_pancake_protocol::{self as proto, v1};
use v1::client_envelope::Body as C;
use v1::server_envelope::Body as S;

use crate::api::{Api, ApiError};
use crate::cache::Cache;
use crate::engine::{Changes, ConversationView, Engine, Error, MessageView};

#[derive(Clone, Debug)]
pub struct ServiceConfig {
    /// `http://host:port` of the app-server.
    pub server: String,
    pub device_name: String,
    pub client_version: String,
    /// Directory for the SQLite caches (`None` = in memory only).
    pub cache_dir: Option<PathBuf>,
    /// Messages per conversation sent to the UI.
    pub view_limit: u32,
    /// Certificate trust for `https://` servers (`None` = the public web PKI).
    pub tls: Option<crate::api::TlsConfig>,
}

impl ServiceConfig {
    fn tls(&self) -> crate::api::TlsConfig {
        self.tls.clone().unwrap_or_else(crate::api::web_pki)
    }

    fn api(&self) -> Api {
        Api::with_tls(&self.server, self.tls())
    }
}

#[derive(Debug, Clone)]
pub enum Command {
    Register {
        username: String,
        password: String,
        display_name: String,
    },
    Login {
        username: String,
        password: String,
    },
    /// Continue a stored session; works offline from the cache until the server answers.
    Resume {
        token: String,
        user: v1::User,
    },
    Logout,
    StartCall {
        conversation_id: String,
    },
    JoinCall {
        call_id: String,
    },
    LeaveCall {
        call_id: String,
        participation_id: String,
    },
    DeclineCall {
        call_id: String,
    },
    /// Find a user by exact username and open the direct conversation with them.
    StartDirect {
        username: String,
    },
    CreateGroup {
        title: String,
        usernames: Vec<String>,
    },
    RenameGroup {
        conversation_id: String,
        title: String,
    },
    AddGroupMember {
        conversation_id: String,
        username: String,
    },
    RemoveGroupMember {
        conversation_id: String,
        user_id: String,
    },
    /// The conversation on screen (`None` = none): its messages are marked read.
    Open {
        conversation_id: Option<String>,
    },
    Send {
        conversation_id: String,
        body: String,
        reply_to_id: String,
    },
    Edit {
        conversation_id: String,
        message_id: String,
        body: String,
    },
    Delete {
        conversation_id: String,
        message_id: String,
    },
    Retry {
        nonce: String,
    },
    Discard {
        nonce: String,
    },
    SaveDraft {
        conversation_id: String,
        body: String,
        reply_to_id: String,
    },
    Block {
        user_id: String,
        blocked: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    SignedOut,
    /// Signed in, connecting (first connect or after a drop); data comes from the cache.
    Connecting,
    Online,
    /// Lost the server; next attempt in this many milliseconds.
    Reconnecting {
        retry_in_ms: u64,
    },
}

#[derive(Debug, Clone)]
pub enum Update {
    Calls(Vec<v1::ConversationCall>),
    CallEvent(v1::ConversationCall),
    /// The media Session consumes the token directly; never pass it to UI models/logs.
    CallReady {
        participation: v1::CallParticipation,
        token: String,
    },
    Status(Status),
    /// Store the token (Credential Manager) and the user for `Resume`.
    SignedIn {
        user: v1::User,
        token: String,
    },
    /// Forget the stored token.
    SignedOut {
        reason: String,
    },
    Conversations(Vec<ConversationView>),
    Messages {
        conversation_id: String,
        messages: Vec<MessageView>,
    },
    /// `StartDirect` found the user: show this conversation.
    Opened {
        conversation_id: String,
    },
    /// Membership was removed: clear the open chat, including its composer.
    Closed {
        conversation_id: String,
    },
    /// A command failed (shown to the user; never contains secrets).
    Error(String),
}

pub struct Service {
    tx: mpsc::UnboundedSender<Command>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Service {
    pub fn start(cfg: ServiceConfig, on_update: impl Fn(Update) + Send + 'static) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("messenger".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("messenger runtime");
                rt.block_on(Worker::new(cfg, Box::new(on_update)).run(rx, stop_rx));
            })
            .expect("messenger thread");
        Self {
            tx,
            stop: Some(stop_tx),
            thread: Some(thread),
        }
    }

    /// Never blocks.
    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const MIN_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(5);

struct Worker {
    cfg: ServiceConfig,
    out: Box<dyn Fn(Update) + Send>,
    engine: Option<Engine>,
    ws: Option<Ws>,
    heartbeat: Duration,
    next_ping: Instant,
    next_connect: Instant,
    backoff: Duration,
}

impl Worker {
    fn new(cfg: ServiceConfig, out: Box<dyn Fn(Update) + Send>) -> Self {
        Self {
            cfg,
            out,
            engine: None,
            ws: None,
            heartbeat: Duration::from_secs(10),
            next_ping: Instant::now(),
            next_connect: Instant::now(),
            backoff: MIN_BACKOFF,
        }
    }

    async fn run(
        mut self,
        mut rx: mpsc::UnboundedReceiver<Command>,
        mut stop: tokio::sync::oneshot::Receiver<()>,
    ) {
        (self.out)(Update::Status(Status::SignedOut));
        loop {
            if self.engine.is_some() && self.ws.is_none() && Instant::now() >= self.next_connect {
                self.connect().await;
            }
            let wake = if self.ws.is_some() {
                self.next_ping
            } else if self.engine.is_some() {
                self.next_connect
            } else {
                Instant::now() + Duration::from_secs(3600)
            };
            let ws = &mut self.ws;
            let frame = async {
                match ws {
                    Some(ws) => ws.next().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = &mut stop => break,
                cmd = rx.recv() => match cmd {
                    Some(c) => self.command(c).await,
                    None => break,
                },
                f = frame => self.frame(f).await,
                _ = tokio::time::sleep_until(wake) => {
                    if self.ws.is_some() {
                        self.ping().await;
                    }
                }
            }
        }
        if let Some(mut ws) = self.ws.take() {
            let _ = ws.close(None).await;
        }
    }

    fn emit(&self, u: Update) {
        (self.out)(u);
    }

    fn error(&self, e: &Error) {
        self.emit(Update::Error(e.to_string()));
    }

    /// Sends the views that `ch` touched.
    fn push(&self, ch: &Changes) {
        let Some(engine) = &self.engine else { return };
        if ch.calls {
            self.emit(Update::Calls(engine.calls.clone()));
        }
        if let Some(id) = &ch.closed {
            self.emit(Update::Closed {
                conversation_id: id.clone(),
            });
        }
        // Also invalidate previously displayed histories for removed conversations.
        for id in &ch.conversations {
            if matches!(engine.cache.conversation(id), Ok(None)) {
                self.emit(Update::Messages {
                    conversation_id: id.clone(),
                    messages: Vec::new(),
                });
            }
        }
        if ch.list {
            match engine.conversation_views() {
                Ok(v) => self.emit(Update::Conversations(v)),
                Err(e) => self.error(&e),
            }
        }
        if let Some(open) = engine.open_conversation()
            && (ch.conversations.contains(open) || ch.list)
        {
            self.push_messages(open);
        }
    }

    fn push_messages(&self, conv: &str) {
        let Some(engine) = &self.engine else { return };
        match engine.message_views(conv, self.cfg.view_limit) {
            Ok(messages) => self.emit(Update::Messages {
                conversation_id: conv.to_owned(),
                messages,
            }),
            Err(e) => self.error(&e),
        }
    }

    /// Handles the outcome of engine work: pushes views, or signs out on a dead session.
    fn settle(&mut self, r: Result<Changes, Error>) -> bool {
        match r {
            Ok(ch) if ch.signed_out => {
                self.sign_out("this session was ended on another device", true);
                false
            }
            Ok(ch) => {
                self.push(&ch);
                true
            }
            Err(e) if e.is_unauthenticated() => {
                self.sign_out("session ended, sign in again", true);
                false
            }
            // Offline: the operation stays queued; the status line says why.
            Err(Error::Api(ApiError::Network(_))) => true,
            Err(e) => {
                self.error(&e);
                true
            }
        }
    }

    fn open_cache(&self, user_id: &str) -> Cache {
        let Some(dir) = &self.cfg.cache_dir else {
            return Cache::in_memory().expect("in-memory cache");
        };
        let path = dir
            .join(server_key(&self.cfg.server))
            .join(format!("{user_id}.sqlite3"));
        match Cache::open(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "cache unavailable, using memory");
                self.emit(Update::Error(format!("local cache unavailable: {e}")));
                Cache::in_memory().expect("in-memory cache")
            }
        }
    }

    fn signed_in(&mut self, auth: v1::AuthResponse) {
        let user = auth.user.clone().unwrap_or_default();
        let mut api = self.cfg.api();
        api.set_token(Some(auth.token.clone()));
        let cache = self.open_cache(&user.user_id);
        self.engine = Some(Engine::new(api, cache, user.clone()));
        self.emit(Update::SignedIn {
            user,
            token: auth.token,
        });
        self.push(&Changes {
            list: true,
            ..Default::default()
        });
        self.next_connect = Instant::now();
        self.backoff = MIN_BACKOFF;
        self.emit(Update::Status(Status::Connecting));
    }

    /// Ends the signed-in state. `wipe` deletes the local cache (logout, revoked session:
    /// the history must not stay readable on this computer).
    fn sign_out(&mut self, reason: &str, wipe: bool) {
        self.ws = None;
        if let Some(engine) = self.engine.take() {
            let path = engine.cache.path().map(|p| p.to_owned());
            drop(engine);
            if let (true, Some(p)) = (wipe, path) {
                for ext in ["sqlite3", "sqlite3-wal", "sqlite3-shm"] {
                    let _ = std::fs::remove_file(p.with_extension(ext));
                }
            }
        }
        self.emit(Update::SignedOut {
            reason: reason.to_owned(),
        });
        self.emit(Update::Status(Status::SignedOut));
    }

    async fn command(&mut self, cmd: Command) {
        let device = self.cfg.device_name.clone();
        match cmd {
            Command::Register {
                username,
                password,
                display_name,
            } => {
                let api = self.cfg.api();
                let r = api
                    .register(&v1::RegisterRequest {
                        username,
                        password,
                        display_name,
                        device_name: device,
                    })
                    .await;
                match r {
                    Ok(auth) => self.signed_in(auth),
                    Err(e) => self.emit(Update::Error(e.to_string())),
                }
            }
            Command::Login { username, password } => {
                let api = self.cfg.api();
                let r = api
                    .login(&v1::LoginRequest {
                        username,
                        password,
                        device_name: device,
                    })
                    .await;
                match r {
                    Ok(auth) => self.signed_in(auth),
                    Err(e) => self.emit(Update::Error(e.to_string())),
                }
            }
            Command::Resume { token, user } => {
                let mut api = self.cfg.api();
                api.set_token(Some(token.clone()));
                match api.me().await {
                    Ok(me) => self.signed_in(v1::AuthResponse {
                        token,
                        user: Some(me),
                        ..Default::default()
                    }),
                    Err(e) if e.is_unauthenticated() => {
                        self.emit(Update::SignedOut {
                            reason: "session ended, sign in again".into(),
                        });
                    }
                    // Offline start: show the cache, keep trying to connect.
                    Err(_) => self.signed_in(v1::AuthResponse {
                        token,
                        user: Some(user),
                        ..Default::default()
                    }),
                }
            }
            Command::Logout => {
                if let Some(engine) = &self.engine {
                    let _ = engine.api.logout().await;
                }
                self.sign_out("signed out", true);
            }
            cmd @ (Command::StartCall { .. }
            | Command::JoinCall { .. }
            | Command::LeaveCall { .. }
            | Command::DeclineCall { .. }) => {
                let Some(engine) = self.engine.as_mut() else {
                    self.emit(Update::Error("sign in first".into()));
                    return;
                };
                let ready = match cmd {
                    Command::StartCall { conversation_id } => {
                        engine.api.create_call(&conversation_id).await.map(Some)
                    }
                    Command::JoinCall { call_id } => engine.api.join_call(&call_id).await.map(Some),
                    Command::LeaveCall {
                        call_id,
                        participation_id,
                    } => engine
                        .api
                        .leave_call(&call_id, &participation_id)
                        .await
                        .map(|_| None),
                    Command::DeclineCall { call_id } => {
                        engine.api.decline_call(&call_id).await.map(|_| None)
                    }
                    _ => unreachable!(),
                };
                let token = engine.api.token().unwrap_or_default().to_owned();
                match ready {
                    Ok(Some(participation)) => self.emit(Update::CallReady {
                        participation,
                        token,
                    }),
                    Ok(None) => {}
                    Err(e) => {
                        self.settle(Err(e.into()));
                        return;
                    }
                }
                let r = self.engine.as_mut().unwrap().refresh_calls().await;
                self.settle(r);
            }
            other => {
                let Some(engine) = self.engine.as_mut() else {
                    self.emit(Update::Error("sign in first".into()));
                    return;
                };
                let r = Self::engine_command(engine, other).await;
                if let Ok((Some(conv), _)) = &r {
                    self.emit(Update::Opened {
                        conversation_id: conv.clone(),
                    });
                }
                self.settle(r.map(|(_, ch)| ch));
            }
        }
    }

    /// Commands of a signed-in user; returns a conversation to show, if any.
    async fn engine_command(
        engine: &mut Engine,
        cmd: Command,
    ) -> Result<(Option<String>, Changes), Error> {
        let mut ch = Changes::default();
        let conv_changed = |id: &str| Changes {
            list: true,
            conversations: [id.to_owned()].into(),
            signed_out: false,
            closed: None,
            calls: false,
        };
        match cmd {
            Command::StartDirect { username } => {
                let (id, c) = engine
                    .start_direct(username.trim().trim_start_matches('@'))
                    .await?;
                ch.merge(c);
                ch.merge(engine.open(Some(&id)).await?);
                ch.merge(conv_changed(&id));
                return Ok((Some(id), ch));
            }
            Command::CreateGroup { title, usernames } => {
                let usernames: Vec<_> = usernames
                    .iter()
                    .map(|u| u.trim().trim_start_matches('@').to_owned())
                    .collect();
                let (id, c) = engine.create_group(&title, &usernames).await?;
                ch.merge(c);
                ch.merge(engine.open(Some(&id)).await?);
                ch.merge(conv_changed(&id));
                return Ok((Some(id), ch));
            }
            Command::RenameGroup {
                conversation_id,
                title,
            } => {
                ch.merge(engine.rename_group(&conversation_id, &title).await?);
            }
            Command::AddGroupMember {
                conversation_id,
                username,
            } => {
                ch.merge(
                    engine
                        .add_group_member(&conversation_id, username.trim().trim_start_matches('@'))
                        .await?,
                );
            }
            Command::RemoveGroupMember {
                conversation_id,
                user_id,
            } => {
                ch.merge(
                    engine
                        .remove_group_member(&conversation_id, &user_id)
                        .await?,
                );
            }
            Command::Open { conversation_id } => {
                ch.merge(
                    engine
                        .open(conversation_id.as_deref())
                        .await
                        .unwrap_or_default(),
                );
                if let Some(id) = conversation_id {
                    ch.merge(conv_changed(&id));
                }
            }
            Command::Send {
                conversation_id,
                body,
                reply_to_id,
            } => {
                engine.queue_send(&conversation_id, &body, &reply_to_id)?;
                ch.merge(conv_changed(&conversation_id));
                ch.merge(engine.flush_pending().await?);
            }
            Command::Edit {
                conversation_id,
                message_id,
                body,
            } => {
                engine.queue_edit(&conversation_id, &message_id, &body)?;
                ch.merge(conv_changed(&conversation_id));
                ch.merge(engine.flush_pending().await?);
            }
            Command::Delete {
                conversation_id,
                message_id,
            } => {
                engine.queue_delete(&conversation_id, &message_id)?;
                ch.merge(conv_changed(&conversation_id));
                ch.merge(engine.flush_pending().await?);
            }
            Command::Retry { nonce } => {
                engine.retry(&nonce)?;
                ch.list = true;
                ch.merge(engine.flush_pending().await?);
            }
            Command::Discard { nonce } => {
                engine.discard(&nonce)?;
                ch.list = true;
            }
            Command::SaveDraft {
                conversation_id,
                body,
                reply_to_id,
            } => {
                engine.save_draft(&conversation_id, &body, &reply_to_id)?;
            }
            Command::Block { user_id, blocked } => {
                ch.merge(engine.set_blocked(&user_id, blocked).await?);
            }
            Command::Register { .. }
            | Command::Login { .. }
            | Command::Resume { .. }
            | Command::Logout => {}
            Command::StartCall { .. }
            | Command::JoinCall { .. }
            | Command::LeaveCall { .. }
            | Command::DeclineCall { .. } => unreachable!("handled by worker"),
        }
        Ok((None, ch))
    }

    async fn connect(&mut self) {
        let Some(engine) = &self.engine else { return };
        let token = engine.api.token().unwrap_or_default().to_owned();
        let url = format!(
            "{}/v1/signal",
            self.cfg
                .server
                .replacen("http://", "ws://", 1)
                .replacen("https://", "wss://", 1)
        );
        let attempt = async {
            let connector = tokio_tungstenite::Connector::Rustls(self.cfg.tls());
            let (mut ws, _) = tokio_tungstenite::connect_async_tls_with_config(
                url.as_str(),
                None,
                false,
                Some(connector),
            )
            .await
            .map_err(|e| crate::api::describe(&e))?;
            let hello = proto::client(
                1,
                C::Hello(v1::Hello {
                    client_version: self.cfg.client_version.clone(),
                    display_name: String::new(),
                    auth_token: token,
                }),
            );
            ws.send(Message::Binary(proto::encode(&hello).into()))
                .await
                .map_err(|_| "connection lost".to_owned())?;
            loop {
                match ws.next().await {
                    Some(Ok(Message::Binary(b))) => {
                        match proto::decode_server(&b).map(|e| e.body) {
                            Ok(Some(S::Welcome(w))) => return Ok((ws, w)),
                            Ok(Some(S::Error(e)))
                                if e.code == v1::ErrorCode::Unauthenticated as i32 =>
                            {
                                return Err("unauthenticated".to_owned());
                            }
                            Ok(Some(S::Error(e))) => return Err(e.message),
                            _ => {}
                        }
                    }
                    Some(Ok(_)) => {}
                    _ => return Err("connection lost".to_owned()),
                }
            }
        };
        match tokio::time::timeout(Duration::from_secs(10), attempt).await {
            Ok(Ok((ws, welcome))) => {
                self.heartbeat =
                    Duration::from_millis(u64::from(welcome.heartbeat_interval_ms.max(1000)));
                self.next_ping = Instant::now() + self.heartbeat;
                self.ws = Some(ws);
                self.backoff = MIN_BACKOFF;
                self.emit(Update::Status(Status::Online));
                // Resync after every connect: whatever happened while away.
                let r = match self.engine.as_mut() {
                    Some(e) => e.sync().await,
                    None => return,
                };
                // A dead session signs out inside `settle`.
                self.settle(r);
            }
            Ok(Err(e)) if e == "unauthenticated" => {
                self.sign_out("session ended, sign in again", true);
            }
            Ok(Err(e)) if e == crate::api::UNTRUSTED_CERTIFICATE => {
                // Never connects until the trust is fixed: say so, keep retrying slowly.
                self.emit(Update::Error(e));
                self.disconnected();
            }
            _ => self.disconnected(),
        }
    }

    fn disconnected(&mut self) {
        self.ws = None;
        self.next_connect = Instant::now() + self.backoff;
        self.emit(Update::Status(Status::Reconnecting {
            retry_in_ms: self.backoff.as_millis() as u64,
        }));
        self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
    }

    async fn frame(&mut self, f: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>) {
        match f {
            Some(Ok(Message::Binary(b))) => {
                let Ok(env) = proto::decode_server(&b) else {
                    return;
                };
                let Some(body) = env.body else { return };
                if matches!(body, S::Pong(_)) {
                    return;
                }
                if let S::CallStarted(c)
                | S::CallParticipantJoined(c)
                | S::CallParticipantLeft(c)
                | S::CallEnded(c) = &body
                {
                    self.emit(Update::CallEvent(c.clone()));
                }
                let r = match self.engine.as_mut() {
                    Some(e) => e.on_event(body).await,
                    None => return,
                };
                self.settle(r);
            }
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => self.disconnected(),
            Some(Ok(_)) => {}
        }
    }

    async fn ping(&mut self) {
        self.next_ping = Instant::now() + self.heartbeat;
        let Some(ws) = &mut self.ws else { return };
        let ping = proto::client(0, C::Ping(v1::Ping { client_time_us: 0 }));
        if ws
            .send(Message::Binary(proto::encode(&ping).into()))
            .await
            .is_err()
        {
            self.disconnected();
        }
    }
}

/// Directory name for one server's caches (no address in clear: it may be a private IP).
fn server_key(server: &str) -> String {
    // FNV-1a 64: stable across builds, no extra dependency.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in server.trim_end_matches('/').bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("server-{h:016x}")
}
