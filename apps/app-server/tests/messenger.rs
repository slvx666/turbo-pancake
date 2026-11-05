//! Messenger integration and security tests (S5, ADR 0013): a real server over TCP,
//! HTTP + WebSocket, against a real PostgreSQL (crates/pg-test).

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use prost::Message as _;
use reqwest::{Method, StatusCode};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use turbo_pancake_app_server::Config;
use turbo_pancake_app_server::ratelimit::{Limits, Rate};
use turbo_pancake_protocol::v1::{
    self, ApiErrorCode, ErrorCode, client_envelope::Body as C, server_envelope::Body as S,
};
use turbo_pancake_protocol::{API_CONTENT_TYPE, client, decode_server, encode};

const STEP: Duration = Duration::from_secs(10);

struct Server {
    addr: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Server {
    async fn start(db: &str, tweak: impl FnOnce(&mut Config)) -> Self {
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_env_filter("warn")
            .try_init();
        let mut config = Config {
            database_url: Some(db.to_owned()),
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
        // Ready once the version endpoint answers (migrations ran before listening).
        let api = Api::new(addr);
        for _ in 0..200 {
            if api.http.get(api.url("/v1/version")).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Self {
            addr,
            stop: Some(tx),
            task,
        }
    }

    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        let _ = tokio::time::timeout(STEP, self.task).await;
    }

    fn api(&self) -> Api {
        Api::new(self.addr)
    }
}

fn database() -> Option<String> {
    let url = turbo_pancake_pg_test::database_url();
    if url.is_none() {
        eprintln!("skipped: no PostgreSQL");
    }
    url
}

#[derive(Clone)]
struct Api {
    http: reqwest::Client,
    base: String,
}

type Failure = (StatusCode, v1::ApiError);

impl Api {
    fn new(addr: SocketAddr) -> Self {
        Self {
            // reqwest is built without a default crypto provider (ADR 0014): give it the
            // same rustls config the clients use.
            http: reqwest::Client::builder()
                .use_preconfigured_tls((*turbo_pancake_tls::client_config(None).unwrap()).clone())
                .build()
                .unwrap(),
            base: format!("http://{addr}"),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn raw(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<Vec<u8>>,
    ) -> (StatusCode, Vec<u8>) {
        let mut req = self.http.request(method, self.url(path));
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        if let Some(b) = body {
            req = req.header("content-type", API_CONTENT_TYPE).body(b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        (status, resp.bytes().await.unwrap().to_vec())
    }

    async fn call<R: prost::Message + Default>(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<&(dyn ErasedMessage + Sync)>,
    ) -> Result<R, Failure> {
        let (status, bytes) = self
            .raw(method, path, token, body.map(ErasedMessage::bytes))
            .await;
        if status.is_success() {
            Ok(R::decode(bytes.as_slice()).expect("response body"))
        } else {
            Err((
                status,
                v1::ApiError::decode(bytes.as_slice()).expect("ApiError body"),
            ))
        }
    }

    async fn register(&self, name: &str) -> v1::AuthResponse {
        self.call(
            Method::POST,
            "/v1/auth/register",
            None,
            Some(&v1::RegisterRequest {
                username: name.into(),
                password: "correct horse".into(),
                display_name: format!("{name} display"),
                device_name: "test".into(),
            }),
        )
        .await
        .expect("register")
    }

    async fn login(&self, name: &str, password: &str) -> Result<v1::AuthResponse, Failure> {
        self.call(
            Method::POST,
            "/v1/auth/login",
            None,
            Some(&v1::LoginRequest {
                username: name.into(),
                password: password.into(),
                device_name: "test 2".into(),
            }),
        )
        .await
    }

    async fn direct(&self, token: &str, user_id: &str) -> v1::Conversation {
        self.call(
            Method::POST,
            "/v1/conversations/direct",
            Some(token),
            Some(&v1::CreateDirectRequest {
                user_id: user_id.into(),
            }),
        )
        .await
        .expect("create direct")
    }

    async fn send(
        &self,
        token: &str,
        conv: &str,
        nonce: &str,
        body: &str,
    ) -> Result<v1::ChatMessage, Failure> {
        self.call(
            Method::POST,
            &format!("/v1/conversations/{conv}/messages"),
            Some(token),
            Some(&v1::SendMessageRequest {
                client_nonce: nonce.into(),
                body: body.into(),
                reply_to_id: String::new(),
            }),
        )
        .await
    }

    async fn changes(&self, token: &str, conv: &str, after: i64) -> v1::MessagePage {
        self.call(
            Method::GET,
            &format!("/v1/conversations/{conv}/changes?after={after}"),
            Some(token),
            None,
        )
        .await
        .expect("changes")
    }

    async fn conversations(&self, token: &str) -> Vec<v1::Conversation> {
        self.call::<v1::ConversationList>(Method::GET, "/v1/conversations", Some(token), None)
            .await
            .expect("conversations")
            .conversations
    }
}

/// `&dyn` over prost messages for the helper above.
trait ErasedMessage {
    fn bytes(&self) -> Vec<u8>;
}
impl<M: prost::Message> ErasedMessage for M {
    fn bytes(&self) -> Vec<u8> {
        self.encode_to_vec()
    }
}

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Live {
    ws: Ws,
    welcome: v1::Welcome,
}

impl Live {
    async fn attach_call(&mut self, call: &v1::CallParticipation) -> bool {
        let req = client(
            900,
            C::JoinCallMedia(v1::JoinCallMedia {
                call_id: call.call.as_ref().unwrap().call_id.clone(),
                participation_id: call.participation_id.clone(),
            }),
        );
        self.ws
            .send(Message::Binary(encode(&req).into()))
            .await
            .unwrap();
        self.expect(|body| match body {
            S::RoomJoined(_) => Some(true),
            S::Error(_) => Some(false),
            _ => None,
        })
        .await
    }
    async fn connect(addr: SocketAddr, token: &str) -> Result<Self, v1::Error> {
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/v1/signal"))
            .await
            .unwrap();
        let hello = client(
            1,
            C::Hello(v1::Hello {
                client_version: "test".into(),
                display_name: if token.is_empty() {
                    "anon".into()
                } else {
                    String::new()
                },
                auth_token: token.into(),
            }),
        );
        ws.send(Message::Binary(encode(&hello).into()))
            .await
            .unwrap();
        let mut live = Self {
            ws,
            welcome: Default::default(),
        };
        match live.next().await {
            Some(S::Welcome(w)) => {
                live.welcome = w;
                Ok(live)
            }
            Some(S::Error(e)) => Err(e),
            other => panic!("expected Welcome, got {other:?}"),
        }
    }

    /// Next server frame body before `deadline`: `Err(())` on timeout, `Ok(None)` when
    /// the server closed the connection. Pings while waiting, like a real client.
    async fn next_until(&mut self, deadline: tokio::time::Instant) -> Result<Option<S>, ()> {
        loop {
            let tick = tokio::time::sleep(Duration::from_millis(200));
            tokio::select! {
                msg = self.ws.next() => match msg {
                    Some(Ok(Message::Binary(b))) => {
                        let body = decode_server(&b).unwrap().body.unwrap();
                        if !matches!(body, S::Pong(_)) {
                            return Ok(Some(body));
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return Ok(None),
                    Some(Ok(_)) => {}
                },
                _ = tokio::time::sleep_until(deadline) => return Err(()),
                _ = tick => {
                    let ping = client(0, C::Ping(v1::Ping { client_time_us: 1 }));
                    let _ = self.ws.send(Message::Binary(encode(&ping).into())).await;
                }
            }
        }
    }

    async fn next(&mut self) -> Option<S> {
        self.next_until(tokio::time::Instant::now() + STEP)
            .await
            .expect("timed out waiting for a frame")
    }

    /// Next frame matching `pick`, skipping others (presence, conversation updates).
    async fn expect<T>(&mut self, mut pick: impl FnMut(S) -> Option<T>) -> T {
        loop {
            let body = self.next().await.expect("connection closed");
            if let Some(t) = pick(body) {
                return t;
            }
        }
    }

    /// Asserts that nothing matching `pick` arrives within `wait`.
    async fn quiet<T>(&mut self, wait: Duration, mut pick: impl FnMut(&S) -> Option<T>) {
        let end = tokio::time::Instant::now() + wait;
        while let Ok(Some(body)) = self.next_until(end).await {
            assert!(pick(&body).is_none(), "unexpected event {body:?}");
        }
    }
}

fn created(body: S) -> Option<v1::ChatMessage> {
    match body {
        S::MessageCreated(e) => e.message,
        _ => None,
    }
}

// ---- Tests ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn call_http_authentication_invitation_timeout_and_revoked_reservation() {
    let db = database().expect("S6 requires PostgreSQL");
    let srv = Server::start(&db, |_| {}).await;
    let api = srv.api();
    let alice = api.register("alice").await;
    let bob = api.register("bob").await;
    let conv = api
        .direct(&alice.token, &bob.user.as_ref().unwrap().user_id)
        .await;
    let request = v1::CreateCallRequest {
        conversation_id: conv.conversation_id,
    };
    let first: v1::CallParticipation = api
        .call(
            Method::POST,
            "/v1/calls",
            Some(&alice.token),
            Some(&request),
        )
        .await
        .unwrap();
    let id = first.call.as_ref().unwrap().call_id.clone();
    let operations = [
        (Method::GET, "/v1/calls".into(), None),
        (
            Method::POST,
            "/v1/calls".into(),
            Some(request.encode_to_vec()),
        ),
        (Method::POST, format!("/v1/calls/{id}/join"), None),
        (Method::POST, format!("/v1/calls/{id}/decline"), None),
        (
            Method::POST,
            format!("/v1/calls/{id}/leave"),
            Some(
                v1::LeaveCallRequest {
                    participation_id: first.participation_id.clone(),
                }
                .encode_to_vec(),
            ),
        ),
    ];
    for (method, path, body) in operations {
        for token in [None, Some("garbage")] {
            assert_eq!(
                api.raw(method.clone(), &path, token, body.clone()).await.0,
                StatusCode::UNAUTHORIZED
            );
        }
    }
    // Real supervisor and real time: an unanswered/unattached invitation must end.
    tokio::time::timeout(Duration::from_secs(35), async {
        loop {
            let list: v1::CallList = api
                .call(Method::GET, "/v1/calls", Some(&alice.token), None)
                .await
                .unwrap();
            if list.calls.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("unanswered invitation expired");
    let mut live = Live::connect(srv.addr, &alice.token).await.unwrap();
    assert!(!live.attach_call(&first).await);
    let second: v1::CallParticipation = api
        .call(
            Method::POST,
            "/v1/calls",
            Some(&alice.token),
            Some(&request),
        )
        .await
        .unwrap();
    assert_ne!(second.call.unwrap().call_id, id);
    let other = api.login("alice", "correct horse").await.unwrap();
    api.call::<v1::Empty>(
        Method::DELETE,
        &format!("/v1/sessions/{}", alice.session_id),
        Some(&other.token),
        None,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let list: v1::CallList = api
                .call(Method::GET, "/v1/calls", Some(&other.token), None)
                .await
                .unwrap();
            if list.calls.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("revoked unattached reservation released");
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conversation_calls_enforce_account_ownership_blocks_and_removal() {
    let db = database().expect("S6 requires PostgreSQL");
    let srv = Server::start(&db, |_| {}).await;
    let api = srv.api();
    let alice = api.register("alice").await;
    let bob = api.register("bob").await;
    let cara = api.register("cara").await;
    let group: v1::Conversation = api
        .call(
            Method::POST,
            "/v1/conversations/group",
            Some(&alice.token),
            Some(&v1::CreateGroupRequest {
                title: "calls".into(),
                usernames: vec!["bob".into()],
            }),
        )
        .await
        .unwrap();
    let cid = group.conversation_id;
    let first = api
        .send(&alice.token, &cid, "before-call", "history before call")
        .await
        .unwrap();
    let mut a = Live::connect(srv.addr, &alice.token).await.unwrap();
    let mut b = Live::connect(srv.addr, &bob.token).await.unwrap();
    let started: v1::CallParticipation = api
        .call(
            Method::POST,
            "/v1/calls",
            Some(&alice.token),
            Some(&v1::CreateCallRequest {
                conversation_id: cid.clone(),
            }),
        )
        .await
        .unwrap();
    assert!(a.attach_call(&started).await);
    let call_id = started.call.as_ref().unwrap().call_id.clone();
    let joined: v1::CallParticipation = api
        .call(
            Method::POST,
            &format!("/v1/calls/{call_id}/join"),
            Some(&bob.token),
            None,
        )
        .await
        .unwrap();
    assert!(b.attach_call(&joined).await);
    // Non-member cannot observe or join it; anonymous and ordinary room joins cannot bypass it.
    let list: v1::CallList = api
        .call(Method::GET, "/v1/calls", Some(&cara.token), None)
        .await
        .unwrap();
    assert!(list.calls.is_empty());
    assert_eq!(
        api.call::<v1::CallParticipation>(
            Method::POST,
            &format!("/v1/calls/{call_id}/join"),
            Some(&cara.token),
            None
        )
        .await
        .unwrap_err()
        .0,
        StatusCode::NOT_FOUND
    );
    let mut anon = Live::connect(srv.addr, "").await.unwrap();
    assert!(!anon.attach_call(&started).await);
    anon.ws
        .send(Message::Binary(
            encode(&client(
                901,
                C::JoinRoom(v1::JoinRoom {
                    room_id: format!("call_{call_id}"),
                }),
            ))
            .into(),
        ))
        .await
        .unwrap();
    anon.expect(|b| matches!(b, S::Error(_)).then_some(()))
        .await;
    let other = api.login("alice", "correct horse").await.unwrap();
    let mut a2 = Live::connect(srv.addr, &other.token).await.unwrap();
    let moved: v1::CallParticipation = api
        .call(
            Method::POST,
            &format!("/v1/calls/{call_id}/join"),
            Some(&other.token),
            None,
        )
        .await
        .unwrap();
    assert_ne!(moved.participation_id, started.participation_id);
    a.expect(|b| matches!(b, S::RoomLeft(_)).then_some(()))
        .await;
    assert!(a2.attach_call(&moved).await);
    // Repeated join preserves the new lease; stale leave/reattach cannot end or steal it.
    let retry: v1::CallParticipation = api
        .call(
            Method::POST,
            &format!("/v1/calls/{call_id}/join"),
            Some(&other.token),
            None,
        )
        .await
        .unwrap();
    assert_eq!(retry.participation_id, moved.participation_id);
    let _: v1::Empty = api
        .call(
            Method::POST,
            &format!("/v1/calls/{call_id}/leave"),
            Some(&alice.token),
            Some(&v1::LeaveCallRequest {
                participation_id: started.participation_id.clone(),
            }),
        )
        .await
        .unwrap();
    assert!(!a.attach_call(&started).await);
    let list: v1::CallList = api
        .call(Method::GET, "/v1/calls", Some(&bob.token), None)
        .await
        .unwrap();
    assert_eq!(list.calls[0].participants.len(), 2);
    assert_eq!(
        list.calls[0]
            .participants
            .iter()
            .filter(|p| p.user_id == alice.user.as_ref().unwrap().user_id)
            .count(),
        1
    );
    // Accepting a new conversation call evicts old participation, preserving the group for Bob.
    let direct = api
        .direct(&other.token, &cara.user.as_ref().unwrap().user_id)
        .await;
    let d: v1::CallParticipation = api
        .call(
            Method::POST,
            "/v1/calls",
            Some(&other.token),
            Some(&v1::CreateCallRequest {
                conversation_id: direct.conversation_id.clone(),
            }),
        )
        .await
        .unwrap();
    a2.expect(|b| matches!(b, S::RoomLeft(_)).then_some(()))
        .await;
    let list: v1::CallList = api
        .call(Method::GET, "/v1/calls", Some(&bob.token), None)
        .await
        .unwrap();
    assert_eq!(list.calls[0].participants.len(), 1);
    assert_eq!(
        list.calls[0].participants[0].user_id,
        bob.user.as_ref().unwrap().user_id
    );
    // A block ends the direct invitation and prevents either direction of direct calls.
    let _: v1::Empty = api
        .call(
            Method::PUT,
            &format!("/v1/blocks/{}", alice.user.as_ref().unwrap().user_id),
            Some(&cara.token),
            None,
        )
        .await
        .unwrap();
    for token in [&other.token, &cara.token] {
        assert_eq!(
            api.call::<v1::CallParticipation>(
                Method::POST,
                "/v1/calls",
                Some(token),
                Some(&v1::CreateCallRequest {
                    conversation_id: direct.conversation_id.clone()
                })
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::FORBIDDEN
        );
    }
    let d_id = &d.call.as_ref().unwrap().call_id;
    assert_eq!(
        api.call::<v1::CallParticipation>(
            Method::POST,
            &format!("/v1/calls/{d_id}/join"),
            Some(&other.token),
            None
        )
        .await
        .unwrap_err()
        .0,
        StatusCode::NOT_FOUND
    );
    // Removing the last group caller ends the call and its media authorization.
    let _: v1::Conversation = api
        .call(
            Method::DELETE,
            &format!(
                "/v1/conversations/{cid}/members/{}",
                bob.user.as_ref().unwrap().user_id
            ),
            Some(&alice.token),
            None,
        )
        .await
        .unwrap();
    b.expect(|b| matches!(b, S::RoomLeft(_)).then_some(()))
        .await;
    assert!(!b.attach_call(&joined).await);
    let list: v1::CallList = api
        .call(Method::GET, "/v1/calls", Some(&alice.token), None)
        .await
        .unwrap();
    assert!(list.calls.is_empty());
    api.send(&alice.token, &cid, "after-call", "history after call")
        .await
        .unwrap();
    let history = api.changes(&alice.token, &cid, 0).await;
    assert_eq!(history.messages.len(), 2);
    assert_eq!(history.messages[0].message_id, first.message_id);
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn calls_concurrent_devices_stale_leave_decline_busy_and_restart() {
    let db = database().expect("S6 requires PostgreSQL");
    let srv = Server::start(&db, |_| {}).await;
    let api = srv.api();
    let alice = api.register("alice").await;
    let bob = api.register("bob").await;
    let cara = api.register("cara").await;
    let group: v1::Conversation = api
        .call(
            Method::POST,
            "/v1/conversations/group",
            Some(&alice.token),
            Some(&v1::CreateGroupRequest {
                title: "call race".into(),
                usernames: vec!["bob".into()],
            }),
        )
        .await
        .unwrap();
    let req = v1::CreateCallRequest {
        conversation_id: group.conversation_id.clone(),
    };
    let a2 = api.login("alice", "correct horse").await.unwrap();
    let a3 = api.login("alice", "correct horse").await.unwrap();
    let (x, y, z) = tokio::join!(
        api.call::<v1::CallParticipation>(
            Method::POST,
            "/v1/calls",
            Some(&alice.token),
            Some(&req)
        ),
        api.call::<v1::CallParticipation>(Method::POST, "/v1/calls", Some(&a2.token), Some(&req)),
        api.call::<v1::CallParticipation>(Method::POST, "/v1/calls", Some(&a3.token), Some(&req))
    );
    let participants = [x.unwrap(), y.unwrap(), z.unwrap()];
    let tokens = [&alice.token, &a2.token, &a3.token];
    let list: v1::CallList = api
        .call(Method::GET, "/v1/calls", Some(&bob.token), None)
        .await
        .unwrap();
    assert_eq!(list.calls.len(), 1);
    assert_eq!(list.calls[0].participants.len(), 1);
    let mut winners = Vec::new();
    let mut connections = Vec::new();
    for i in 0..3 {
        let mut ws = Live::connect(srv.addr, tokens[i]).await.unwrap();
        if ws.attach_call(&participants[i]).await {
            winners.push(i);
        }
        connections.push(ws);
    }
    assert_eq!(winners.len(), 1, "only the final owning device can attach");
    for i in (0..3).filter(|i| *i != winners[0]) {
        let id = &participants[i].call.as_ref().unwrap().call_id;
        let _: v1::Empty = api
            .call(
                Method::POST,
                &format!("/v1/calls/{id}/leave"),
                Some(tokens[i]),
                Some(&v1::LeaveCallRequest {
                    participation_id: participants[i].participation_id.clone(),
                }),
            )
            .await
            .unwrap();
    }
    let direct = api
        .direct(&cara.token, &alice.user.as_ref().unwrap().user_id)
        .await;
    assert_eq!(
        api.call::<v1::CallParticipation>(
            Method::POST,
            "/v1/calls",
            Some(&cara.token),
            Some(&v1::CreateCallRequest {
                conversation_id: direct.conversation_id.clone()
            })
        )
        .await
        .unwrap_err()
        .0,
        StatusCode::CONFLICT
    );
    let winner = winners[0];
    let id = &participants[winner].call.as_ref().unwrap().call_id;
    let _: v1::Empty = api
        .call(
            Method::POST,
            &format!("/v1/calls/{id}/leave"),
            Some(tokens[winner]),
            Some(&v1::LeaveCallRequest {
                participation_id: participants[winner].participation_id.clone(),
            }),
        )
        .await
        .unwrap();
    let invitation: v1::CallParticipation = api
        .call(
            Method::POST,
            "/v1/calls",
            Some(&cara.token),
            Some(&v1::CreateCallRequest {
                conversation_id: direct.conversation_id,
            }),
        )
        .await
        .unwrap();
    let id = &invitation.call.as_ref().unwrap().call_id;
    let _: v1::Empty = api
        .call(
            Method::POST,
            &format!("/v1/calls/{id}/decline"),
            Some(&alice.token),
            None,
        )
        .await
        .unwrap();
    let list: v1::CallList = api
        .call(Method::GET, "/v1/calls", Some(&cara.token), None)
        .await
        .unwrap();
    assert!(list.calls.is_empty());
    drop(connections);
    srv.stop().await;
    let srv = Server::start(&db, |_| {}).await;
    let list: v1::CallList = srv
        .api()
        .call(Method::GET, "/v1/calls", Some(&alice.token), None)
        .await
        .unwrap();
    assert!(
        list.calls.is_empty(),
        "restart cannot restore dead media sessions"
    );
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_100_members_fanout_capacity_permissions_and_restart() {
    let db = database().expect("S6 requires PostgreSQL");
    let srv = Server::start(&db, |_| {}).await;
    let api = srv.api();
    let mut accounts = Vec::new();
    for i in 0..103 {
        accounts.push(api.register(&format!("member{i:03}")).await);
    }
    let owner = &accounts[0].token;
    let group: v1::Conversation = api
        .call(
            Method::POST,
            "/v1/conversations/group",
            Some(owner),
            Some(&v1::CreateGroupRequest {
                title: "Группа 👋".into(),
                usernames: (1..100).map(|i| format!("MEMBER{i:03}")).collect(),
            }),
        )
        .await
        .unwrap();
    let cid = group.conversation_id;
    assert_eq!(group.kind, v1::ConversationKind::Group as i32);
    assert_eq!(group.members.len(), 100);
    assert_eq!(
        group
            .members
            .iter()
            .filter(|m| m.role == v1::MemberRole::Creator as i32)
            .count(),
        1
    );
    let member_path = format!("/v1/conversations/{cid}/members");
    let conv_path = format!("/v1/conversations/{cid}");
    // All 100 authenticated live connections receive the same real message.
    let mut live = Vec::new();
    for a in &accounts[..100] {
        live.push(Live::connect(srv.addr, &a.token).await.unwrap());
    }
    let first = api
        .send(owner, &cid, "group-first", "Общая история")
        .await
        .unwrap();
    for ws in &mut live {
        assert_eq!(ws.expect(created).await.message_id, first.message_id);
    }
    // Direct blocks don't suppress shared-group messages.
    let blocked_id = &accounts[49].user.as_ref().unwrap().user_id;
    api.call::<v1::Empty>(
        Method::PUT,
        &format!("/v1/blocks/{blocked_id}"),
        Some(owner),
        None,
    )
    .await
    .unwrap();
    api.send(&accounts[49].token, &cid, "group-second", "still in group")
        .await
        .unwrap();
    api.send(&accounts[99].token, &cid, "group-third", "last member")
        .await
        .unwrap();
    for a in &accounts[..100] {
        let page = api.changes(&a.token, &cid, 0).await;
        assert_eq!(page.messages.len(), 3);
        assert_eq!(
            page.messages
                .iter()
                .map(|m| m.server_sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        let receipt: v1::Receipt = api
            .call(
                Method::POST,
                &format!("{conv_path}/read"),
                Some(&a.token),
                Some(&v1::MarkReadRequest {
                    delivered_sequence: 3,
                    read_sequence: 3,
                }),
            )
            .await
            .unwrap();
        assert_eq!(receipt.read_sequence, 3);
    }
    // Non-creators and outsiders cannot manage; outsiders cannot read/write.
    for (token, status) in [
        (&accounts[1].token, StatusCode::FORBIDDEN),
        (&accounts[100].token, StatusCode::NOT_FOUND),
    ] {
        assert_eq!(
            api.call::<v1::Conversation>(
                Method::PATCH,
                &conv_path,
                Some(token),
                Some(&v1::RenameGroupRequest {
                    title: "stolen".into()
                })
            )
            .await
            .unwrap_err()
            .0,
            status
        );
        assert_eq!(
            api.call::<v1::Conversation>(
                Method::POST,
                &member_path,
                Some(token),
                Some(&v1::AddGroupMemberRequest {
                    username: "member100".into()
                })
            )
            .await
            .unwrap_err()
            .0,
            status
        );
        assert_eq!(
            api.call::<v1::Conversation>(
                Method::DELETE,
                &format!("{member_path}/{blocked_id}"),
                Some(token),
                None
            )
            .await
            .unwrap_err()
            .0,
            status
        );
    }
    assert_eq!(
        api.send(&accounts[100].token, &cid, "outsider", "forbidden")
            .await
            .unwrap_err()
            .0,
        StatusCode::NOT_FOUND
    );
    let owner_id = &accounts[0].user.as_ref().unwrap().user_id;
    assert_eq!(
        api.call::<v1::Conversation>(
            Method::DELETE,
            &format!("{member_path}/{owner_id}"),
            Some(owner),
            None
        )
        .await
        .unwrap_err()
        .0,
        StatusCode::FORBIDDEN
    );
    // Idempotent add at capacity; a new member is rejected.
    let same: v1::Conversation = api
        .call(
            Method::POST,
            &member_path,
            Some(owner),
            Some(&v1::AddGroupMemberRequest {
                username: "member001".into(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(same.members.len(), 100);
    assert_eq!(
        api.call::<v1::Conversation>(
            Method::POST,
            &member_path,
            Some(owner),
            Some(&v1::AddGroupMemberRequest {
                username: "member100".into()
            })
        )
        .await
        .unwrap_err()
        .0,
        StatusCode::CONFLICT
    );

    // Remove two members. Drain prior invalidations before checking removal's event.
    for i in [98, 99] {
        let user = &accounts[i].user.as_ref().unwrap().user_id;
        let _: v1::Conversation = api
            .call(
                Method::DELETE,
                &format!("{member_path}/{user}"),
                Some(owner),
                None,
            )
            .await
            .unwrap();
        live[i]
            .expect(|b| match b {
                S::ConversationUpdated(u) if u.conversation_id == cid => Some(()),
                _ => None,
            })
            .await;
        assert!(api.conversations(&accounts[i].token).await.is_empty());
        assert_eq!(
            api.send(&accounts[i].token, &cid, &format!("removed-{i}"), "no")
                .await
                .unwrap_err()
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            api.call::<v1::MessagePage>(
                Method::GET,
                &format!("{conv_path}/changes"),
                Some(&accounts[i].token),
                None
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            api.call::<v1::Receipt>(
                Method::POST,
                &format!("{conv_path}/read"),
                Some(&accounts[i].token),
                Some(&v1::MarkReadRequest {
                    delivered_sequence: 3,
                    read_sequence: 3
                })
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::NOT_FOUND
        );
    }
    // Three concurrent additions compete for two free places.
    let requests = [100, 101, 102].map(|i| v1::AddGroupMemberRequest {
        username: format!("member{i:03}"),
    });
    let (a, b, c) = tokio::join!(
        api.call::<v1::Conversation>(Method::POST, &member_path, Some(owner), Some(&requests[0])),
        api.call::<v1::Conversation>(Method::POST, &member_path, Some(owner), Some(&requests[1])),
        api.call::<v1::Conversation>(Method::POST, &member_path, Some(owner), Some(&requests[2]))
    );
    let results = [a, b, c];
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 2);
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Err((StatusCode::CONFLICT, _))))
            .count(),
        1
    );
    let renamed: v1::Conversation = api
        .call(
            Method::PATCH,
            &conv_path,
            Some(owner),
            Some(&v1::RenameGroupRequest {
                title: "После reconnect".into(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(renamed.members.len(), 100);
    drop(live);
    srv.stop().await;
    let srv = Server::start(&db, |_| {}).await;
    let api = srv.api();
    let snapshot = api.conversations(owner).await;
    assert_eq!(snapshot[0].title, "После reconnect");
    assert_eq!(snapshot[0].members.len(), 100);
    assert_eq!(api.changes(owner, &cid, 0).await.messages.len(), 3);
    assert!(api.conversations(&accounts[99].token).await.is_empty());
    // Re-add restores the same history and receipt cursors, no duplicate rows.
    let user = &accounts[100].user.as_ref().unwrap().user_id;
    let victim = if renamed
        .members
        .iter()
        .any(|m| m.user.as_ref().unwrap().user_id == *user)
    {
        100
    } else {
        101
    };
    let user = &accounts[victim].user.as_ref().unwrap().user_id;
    let _: v1::Conversation = api
        .call(
            Method::DELETE,
            &format!("{member_path}/{user}"),
            Some(owner),
            None,
        )
        .await
        .unwrap();
    let restored: v1::Conversation = api
        .call(
            Method::POST,
            &member_path,
            Some(owner),
            Some(&v1::AddGroupMemberRequest {
                username: "member099".into(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(restored.members.len(), 100);
    assert_eq!(
        restored
            .members
            .iter()
            .find(|m| m.user.as_ref().unwrap().username == "member099")
            .unwrap()
            .read_sequence,
        3
    );
    assert_eq!(
        api.changes(&accounts[99].token, &cid, 0)
            .await
            .messages
            .len(),
        3
    );
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_validation_authentication_and_atomic_creation() {
    let db = database().expect("S6 requires PostgreSQL");
    let srv = Server::start(&db, |_| {}).await;
    let api = srv.api();
    let alice = api.register("alice").await;
    let bob = api.register("bob").await;
    for req in [
        v1::CreateGroupRequest {
            title: " ".into(),
            usernames: vec!["bob".into()],
        },
        v1::CreateGroupRequest {
            title: "ok".into(),
            usernames: vec![],
        },
        v1::CreateGroupRequest {
            title: "ok".into(),
            usernames: vec!["bob".into(); 100],
        },
        v1::CreateGroupRequest {
            title: "ok".into(),
            usernames: vec!["bob".into(), "BOB".into()],
        },
        v1::CreateGroupRequest {
            title: "ok".into(),
            usernames: vec!["alice".into()],
        },
        v1::CreateGroupRequest {
            title: "ok".into(),
            usernames: vec!["bob".into(), "missing".into()],
        },
    ] {
        assert!(
            api.call::<v1::Conversation>(
                Method::POST,
                "/v1/conversations/group",
                Some(&alice.token),
                Some(&req)
            )
            .await
            .is_err()
        );
        assert!(api.conversations(&alice.token).await.is_empty());
        assert!(api.conversations(&bob.token).await.is_empty());
    }
    let direct = api
        .direct(&alice.token, &bob.user.as_ref().unwrap().user_id)
        .await;
    let cid = direct.conversation_id;
    let owner_id = &alice.user.as_ref().unwrap().user_id;
    let actions: Vec<(Method, String, Option<Vec<u8>>)> = vec![
        (
            Method::POST,
            "/v1/conversations/group".into(),
            Some(
                v1::CreateGroupRequest {
                    title: "ok".into(),
                    usernames: vec!["bob".into()],
                }
                .encode_to_vec(),
            ),
        ),
        (
            Method::PATCH,
            format!("/v1/conversations/{cid}"),
            Some(v1::RenameGroupRequest { title: "no".into() }.encode_to_vec()),
        ),
        (
            Method::POST,
            format!("/v1/conversations/{cid}/members"),
            Some(
                v1::AddGroupMemberRequest {
                    username: "bob".into(),
                }
                .encode_to_vec(),
            ),
        ),
        (
            Method::DELETE,
            format!("/v1/conversations/{cid}/members/{owner_id}"),
            None,
        ),
    ];
    for (method, path, body) in &actions {
        for token in [None, Some("garbage")] {
            assert_eq!(
                api.raw(method.clone(), path, token, body.clone()).await.0,
                StatusCode::UNAUTHORIZED
            );
        }
    }
    // Group management never mutates direct conversations, even for their initiator.
    for (method, path, body) in actions.into_iter().skip(1) {
        assert_eq!(
            api.raw(method, &path, Some(&alice.token), body).await.0,
            StatusCode::FORBIDDEN
        );
    }
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_removal_wins_against_a_message_waiting_for_the_row_lock() {
    let db = database().expect("S6 requires PostgreSQL");
    let srv = Server::start(&db, |_| {}).await;
    let api = srv.api();
    let alice = api.register("alice").await;
    let bob = api.register("bob").await;
    let group: v1::Conversation = api
        .call(
            Method::POST,
            "/v1/conversations/group",
            Some(&alice.token),
            Some(&v1::CreateGroupRequest {
                title: "race".into(),
                usernames: vec!["bob".into()],
            }),
        )
        .await
        .unwrap();
    let cid = group.conversation_id;
    let pool = sqlx::PgPool::connect(&db).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM conversations WHERE id = $1::uuid FOR UPDATE")
        .bind(&cid)
        .execute(&mut *tx)
        .await
        .unwrap();
    let task_api = api.clone();
    let token = bob.token.clone();
    let task_cid = cid.clone();
    let task = tokio::spawn(async move {
        task_api
            .send(&token, &task_cid, "race-send", "must not commit")
            .await
    });
    // Observe the actual blocked write; no timing assumption about the HTTP task.
    tokio::time::timeout(STEP, async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname = current_database()
                 AND wait_event_type = 'Lock' AND query LIKE 'UPDATE conversations%')",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if waiting {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("message reached the row lock");
    sqlx::query("UPDATE conversation_members SET left_at = now() WHERE conversation_id = $1::uuid AND user_id = $2::uuid")
        .bind(&cid).bind(&bob.user.as_ref().unwrap().user_id).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(task.await.unwrap().unwrap_err().0, StatusCode::NOT_FOUND);
    let snapshot = api.conversations(&alice.token).await;
    assert_eq!(snapshot[0].last_sequence, 0);
    assert_eq!(snapshot[0].change_seq, 0);
    pool.close().await;
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accounts_sessions_and_passwords() {
    let Some(db) = database() else { return };
    let srv = Server::start(&db, |_| {}).await;
    let api = srv.api();

    let alice = api.register("Alice").await;
    assert_eq!(alice.user.as_ref().unwrap().username, "Alice");
    assert_eq!(alice.user.as_ref().unwrap().display_name, "Alice display");
    assert_eq!(alice.token.len(), 43, "32 random bytes, base64url");

    // Username is unique case-insensitively.
    let dup = api
        .call::<v1::AuthResponse>(
            Method::POST,
            "/v1/auth/register",
            None,
            Some(&v1::RegisterRequest {
                username: "alice".into(),
                password: "another pass".into(),
                ..Default::default()
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(dup.0, StatusCode::CONFLICT);

    // Invalid inputs.
    for (u, p) in [
        ("ab", "correct horse"),
        ("admin", "correct horse"),
        ("bobby", "short"),
    ] {
        let e = api
            .call::<v1::AuthResponse>(
                Method::POST,
                "/v1/auth/register",
                None,
                Some(&v1::RegisterRequest {
                    username: u.into(),
                    password: p.into(),
                    ..Default::default()
                }),
            )
            .await
            .unwrap_err();
        assert_eq!(e.0, StatusCode::BAD_REQUEST, "{u}");
    }

    // Unknown user and wrong password: the same answer (no enumeration).
    let wrong = api.login("alice", "wrong password").await.unwrap_err();
    let unknown = api
        .login("nobody_here", "wrong password")
        .await
        .unwrap_err();
    assert_eq!(wrong.0, StatusCode::UNAUTHORIZED);
    assert_eq!(wrong.0, unknown.0);
    assert_eq!(wrong.1, unknown.1);

    let second = api.login("ALICE", "correct horse").await.unwrap();
    let me: v1::User = api
        .call(Method::GET, "/v1/me", Some(&second.token), None)
        .await
        .unwrap();
    assert_eq!(me.user_id, alice.user.as_ref().unwrap().user_id);

    // Two sessions; each knows which one is current.
    let list: v1::SessionList = api
        .call(Method::GET, "/v1/sessions", Some(&alice.token), None)
        .await
        .unwrap();
    assert_eq!(list.sessions.len(), 2);
    assert_eq!(list.sessions.iter().filter(|s| s.current).count(), 1);

    // Refresh rotates: the old token stops working.
    let refreshed: v1::AuthResponse = api
        .call(Method::POST, "/v1/auth/refresh", Some(&alice.token), None)
        .await
        .unwrap();
    assert_ne!(refreshed.token, alice.token);
    assert_eq!(refreshed.session_id, alice.session_id);
    let old = api
        .call::<v1::User>(Method::GET, "/v1/me", Some(&alice.token), None)
        .await
        .unwrap_err();
    assert_eq!(old.0, StatusCode::UNAUTHORIZED);
    assert_eq!(old.1.code, ApiErrorCode::Unauthenticated as i32);

    // Password change: wrong old password refused; then other sessions end, this one
    // continues with a new token; the old password no longer logs in.
    let bad = api
        .call::<v1::AuthResponse>(
            Method::POST,
            "/v1/me/password",
            Some(&refreshed.token),
            Some(&v1::ChangePasswordRequest {
                old_password: "not it at all".into(),
                new_password: "battery staple".into(),
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(bad.0, StatusCode::FORBIDDEN);
    let mut other_ws = Live::connect(srv.addr, &second.token).await.unwrap();
    let changed: v1::AuthResponse = api
        .call(
            Method::POST,
            "/v1/me/password",
            Some(&refreshed.token),
            Some(&v1::ChangePasswordRequest {
                old_password: "correct horse".into(),
                new_password: "battery staple".into(),
            }),
        )
        .await
        .unwrap();
    other_ws
        .expect(|b| matches!(b, S::SessionRevoked(_)).then_some(()))
        .await;
    assert!(other_ws.next().await.is_none(), "revoked connection closes");
    assert_eq!(
        api.call::<v1::User>(Method::GET, "/v1/me", Some(&second.token), None)
            .await
            .unwrap_err()
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert!(
        api.call::<v1::User>(Method::GET, "/v1/me", Some(&changed.token), None)
            .await
            .is_ok()
    );
    assert!(api.login("alice", "correct horse").await.is_err());
    let third = api.login("alice", "battery staple").await.unwrap();

    // Revoke one session by id; logout the other.
    let _: v1::Empty = api
        .call(
            Method::DELETE,
            &format!("/v1/sessions/{}", third.session_id),
            Some(&changed.token),
            None,
        )
        .await
        .unwrap();
    assert!(
        api.call::<v1::User>(Method::GET, "/v1/me", Some(&third.token), None)
            .await
            .is_err()
    );
    let _: v1::Empty = api
        .call(Method::POST, "/v1/auth/logout", Some(&changed.token), None)
        .await
        .unwrap();
    assert!(
        api.call::<v1::User>(Method::GET, "/v1/me", Some(&changed.token), None)
            .await
            .is_err()
    );
    // A revoked token cannot open a WebSocket either.
    let e = Live::connect(srv.addr, &changed.token).await.err().unwrap();
    assert_eq!(e.code, ErrorCode::Unauthenticated as i32);
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_messages_receipts_edit_delete() {
    let Some(db) = database() else { return };
    let srv = Server::start(&db, |_| {}).await;
    let api = srv.api();
    let alice = api.register("alice").await;
    let bob = api.register("bob").await;
    let carol = api.register("carol").await;
    let alice_id = alice.user.clone().unwrap().user_id;
    let bob_id = bob.user.clone().unwrap().user_id;

    let mut bob_ws = Live::connect(srv.addr, &bob.token).await.unwrap();
    assert_eq!(bob_ws.welcome.user.as_ref().unwrap().user_id, bob_id);

    // Lookup by exact username (case-insensitive), then a direct conversation; creating
    // it again, from either side, returns the same one.
    let found: v1::User = api
        .call(
            Method::GET,
            "/v1/users/lookup?username=BOB",
            Some(&alice.token),
            None,
        )
        .await
        .unwrap();
    assert_eq!(found.user_id, bob_id);
    assert!(found.online, "bob has a live connection");
    let missing = api
        .call::<v1::User>(
            Method::GET,
            "/v1/users/lookup?username=bo",
            Some(&alice.token),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(missing.0, StatusCode::NOT_FOUND);
    let conv = api.direct(&alice.token, &bob_id).await;
    assert_eq!(
        api.direct(&alice.token, &bob_id).await.conversation_id,
        conv.conversation_id
    );
    assert_eq!(
        api.direct(&bob.token, &alice_id).await.conversation_id,
        conv.conversation_id
    );
    assert!(conv.can_send);
    assert_eq!(conv.members.len(), 2);
    let cid = conv.conversation_id.clone();
    bob_ws
        .expect(|b| match b {
            S::ConversationUpdated(c) => Some(c.conversation_id),
            _ => None,
        })
        .await;

    // Send: emoji/Unicode text, server sequence, live event to bob without the nonce.
    let m1 = api
        .send(&alice.token, &cid, "nonce-1", "привет 👋")
        .await
        .unwrap();
    assert_eq!(m1.server_sequence, 1);
    assert_eq!(m1.client_nonce, "nonce-1");
    let ev = bob_ws.expect(created).await;
    assert_eq!(ev.message_id, m1.message_id);
    assert_eq!(ev.body, "привет 👋");
    assert!(ev.client_nonce.is_empty(), "nonce is the author's own");
    assert_eq!(ev.change_seq, m1.change_seq);

    // Retry with the same nonce: the stored message, no new event, no new sequence.
    let (status, bytes) = api
        .raw(
            Method::POST,
            &format!("/v1/conversations/{cid}/messages"),
            Some(&alice.token),
            Some(
                v1::SendMessageRequest {
                    client_nonce: "nonce-1".into(),
                    body: "привет 👋".into(),
                    reply_to_id: String::new(),
                }
                .encode_to_vec(),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        v1::ChatMessage::decode(bytes.as_slice())
            .unwrap()
            .message_id,
        m1.message_id
    );
    bob_ws
        .quiet(Duration::from_millis(300), |b| {
            matches!(b, S::MessageCreated(_)).then_some(())
        })
        .await;

    // Reply from bob.
    let m2: v1::ChatMessage = api
        .call(
            Method::POST,
            &format!("/v1/conversations/{cid}/messages"),
            Some(&bob.token),
            Some(&v1::SendMessageRequest {
                client_nonce: "b-1".into(),
                body: "hi!".into(),
                reply_to_id: m1.message_id.clone(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(m2.server_sequence, 2);
    assert_eq!(m2.reply_to_id, m1.message_id);

    // Only the author edits / deletes; others get 403, non-members 404.
    let path1 = format!("/v1/conversations/{cid}/messages/{}", m1.message_id);
    let e = api
        .call::<v1::ChatMessage>(
            Method::PATCH,
            &path1,
            Some(&bob.token),
            Some(&v1::EditMessageRequest {
                body: "hacked".into(),
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(e.0, StatusCode::FORBIDDEN);
    let e = api
        .call::<v1::ChatMessage>(Method::DELETE, &path1, Some(&bob.token), None)
        .await
        .unwrap_err();
    assert_eq!(e.0, StatusCode::FORBIDDEN);
    for (method, path) in [
        (Method::GET, format!("/v1/conversations/{cid}/messages")),
        (Method::GET, format!("/v1/conversations/{cid}/changes")),
    ] {
        let e = api
            .call::<v1::MessagePage>(method, &path, Some(&carol.token), None)
            .await
            .unwrap_err();
        assert_eq!(e.0, StatusCode::NOT_FOUND);
    }
    assert_eq!(
        api.send(&carol.token, &cid, "c-1", "let me in")
            .await
            .unwrap_err()
            .0,
        StatusCode::NOT_FOUND
    );

    let edited: v1::ChatMessage = api
        .call(
            Method::PATCH,
            &path1,
            Some(&alice.token),
            Some(&v1::EditMessageRequest {
                body: "привет, Боб".into(),
            }),
        )
        .await
        .unwrap();
    assert!(edited.edited_at_ms > 0);
    assert_eq!(edited.server_sequence, 1, "edit keeps the position");
    let ev = bob_ws
        .expect(|b| match b {
            S::MessageEdited(e) => e.message,
            _ => None,
        })
        .await;
    assert_eq!(ev.body, "привет, Боб");

    // Delete for everyone: tombstone in the event, history and change feed.
    let deleted: v1::ChatMessage = api
        .call(Method::DELETE, &path1, Some(&alice.token), None)
        .await
        .unwrap();
    assert!(deleted.deleted && deleted.body.is_empty());
    let ev = bob_ws
        .expect(|b| match b {
            S::MessageDeleted(e) => e.message,
            _ => None,
        })
        .await;
    assert!(ev.deleted && ev.body.is_empty());
    // Idempotent delete; editing a deleted message is gone.
    let again: v1::ChatMessage = api
        .call(Method::DELETE, &path1, Some(&alice.token), None)
        .await
        .unwrap();
    assert_eq!(again.change_seq, deleted.change_seq);
    assert_eq!(
        api.call::<v1::ChatMessage>(
            Method::PATCH,
            &path1,
            Some(&alice.token),
            Some(&v1::EditMessageRequest {
                body: "back".into()
            })
        )
        .await
        .unwrap_err()
        .0,
        StatusCode::NOT_FOUND
    );
    let history: v1::MessagePage = api
        .call(
            Method::GET,
            &format!("/v1/conversations/{cid}/messages"),
            Some(&bob.token),
            None,
        )
        .await
        .unwrap();
    assert_eq!(history.messages.len(), 2);
    assert_eq!(history.messages[0].server_sequence, 2, "newest first");
    assert_eq!(
        history.messages[0].reply_to_id, m1.message_id,
        "reply keeps its link"
    );
    assert!(history.messages[1].deleted && history.messages[1].body.is_empty());

    // Receipts: sending marks the author's own position (bob is at 2 after his reply);
    // alice sends 3, bob reads "up to 99" = 3 (clamped), alice gets read.updated; values
    // never go down.
    let mut alice_ws = Live::connect(srv.addr, &alice.token).await.unwrap();
    api.send(&alice.token, &cid, "nonce-3", "third")
        .await
        .unwrap();
    let r: v1::Receipt = api
        .call(
            Method::POST,
            &format!("/v1/conversations/{cid}/read"),
            Some(&bob.token),
            Some(&v1::MarkReadRequest {
                delivered_sequence: 0,
                read_sequence: 99,
            }),
        )
        .await
        .unwrap();
    assert_eq!((r.delivered_sequence, r.read_sequence), (3, 3));
    let ev = alice_ws
        .expect(|b| match b {
            S::ReadUpdated(r) => Some(r),
            _ => None,
        })
        .await;
    assert_eq!(
        (ev.user_id.as_str(), ev.read_sequence),
        (bob_id.as_str(), 3)
    );
    let r: v1::Receipt = api
        .call(
            Method::POST,
            &format!("/v1/conversations/{cid}/read"),
            Some(&bob.token),
            Some(&v1::MarkReadRequest {
                delivered_sequence: 1,
                read_sequence: 1,
            }),
        )
        .await
        .unwrap();
    assert_eq!(r.read_sequence, 3, "receipts never go back");
    let list = api.conversations(&alice.token).await;
    let bob_member = list[0]
        .members
        .iter()
        .find(|m| m.user.as_ref().unwrap().user_id == bob_id)
        .unwrap();
    assert_eq!(bob_member.read_sequence, 3);
    assert_eq!(list[0].last_sequence, 3);

    // Message limits.
    let e = api
        .send(&alice.token, &cid, "n-big", &"x".repeat(4001))
        .await
        .unwrap_err();
    assert_eq!(e.0, StatusCode::BAD_REQUEST);
    let e = api
        .send(&alice.token, &cid, "n-blank", "   ")
        .await
        .unwrap_err();
    assert_eq!(e.0, StatusCode::BAD_REQUEST);
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocks_are_enforced_by_the_server() {
    let Some(db) = database() else { return };
    let srv = Server::start(&db, |_| {}).await;
    let api = srv.api();
    let alice = api.register("alice").await;
    let bob = api.register("bob").await;
    let alice_id = alice.user.clone().unwrap().user_id;
    let bob_id = bob.user.clone().unwrap().user_id;
    let cid = api.direct(&alice.token, &bob_id).await.conversation_id;
    let first = api.send(&alice.token, &cid, "a-1", "hello").await.unwrap();
    let mut alice_ws = Live::connect(srv.addr, &alice.token).await.unwrap();
    let _bob_ws = Live::connect(srv.addr, &bob.token).await.unwrap();

    // Bob blocks Alice.
    let _: v1::Empty = api
        .call(
            Method::PUT,
            &format!("/v1/blocks/{alice_id}"),
            Some(&bob.token),
            None,
        )
        .await
        .unwrap();
    // Alice is told to refetch and sees bob offline (presence hidden).
    alice_ws
        .expect(|b| match b {
            S::PresenceChanged(p) if p.user_id == bob_id && !p.online => Some(()),
            _ => None,
        })
        .await;
    let e = api
        .send(&alice.token, &cid, "a-2", "are you there?")
        .await
        .unwrap_err();
    assert_eq!(e.0, StatusCode::FORBIDDEN);
    assert_eq!(e.1.message, "you cannot message this user");
    // The blocker cannot send either; the answer does not say who blocked whom.
    let e2 = api.send(&bob.token, &cid, "b-1", "bye").await.unwrap_err();
    assert_eq!(e2.1, e.1);
    // Editing old messages is a new contact too; deleting own messages stays possible.
    let path = format!("/v1/conversations/{cid}/messages/{}", first.message_id);
    assert_eq!(
        api.call::<v1::ChatMessage>(
            Method::PATCH,
            &path,
            Some(&alice.token),
            Some(&v1::EditMessageRequest { body: "x".into() })
        )
        .await
        .unwrap_err()
        .0,
        StatusCode::FORBIDDEN
    );
    let a_view = &api.conversations(&alice.token).await[0];
    assert!(!a_view.can_send && !a_view.blocked_by_me);
    let b_view = &api.conversations(&bob.token).await[0];
    assert!(!b_view.can_send && b_view.blocked_by_me);
    let bob_seen_by_alice = a_view
        .members
        .iter()
        .find(|m| m.user.as_ref().unwrap().user_id == bob_id)
        .unwrap();
    assert!(
        !bob_seen_by_alice.user.as_ref().unwrap().online,
        "presence hidden"
    );
    let looked_up: v1::User = api
        .call(
            Method::GET,
            "/v1/users/lookup?username=bob",
            Some(&alice.token),
            None,
        )
        .await
        .unwrap();
    assert!(!looked_up.online);
    let blocks: v1::UserList = api
        .call(Method::GET, "/v1/blocks", Some(&bob.token), None)
        .await
        .unwrap();
    assert_eq!(blocks.users.len(), 1);
    // A retried send of a message sent before the block still answers with it.
    assert_eq!(
        api.send(&alice.token, &cid, "a-1", "hello")
            .await
            .unwrap()
            .message_id,
        first.message_id
    );
    // Blocking yourself, unknown users, malformed ids.
    assert_eq!(
        api.call::<v1::Empty>(
            Method::PUT,
            &format!("/v1/blocks/{bob_id}"),
            Some(&bob.token),
            None
        )
        .await
        .unwrap_err()
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        api.call::<v1::Empty>(
            Method::PUT,
            "/v1/blocks/00000000-0000-4000-8000-000000000000",
            Some(&bob.token),
            None
        )
        .await
        .unwrap_err()
        .0,
        StatusCode::NOT_FOUND
    );

    // Unblock: sending works again and presence comes back.
    let _: v1::Empty = api
        .call(
            Method::DELETE,
            &format!("/v1/blocks/{alice_id}"),
            Some(&bob.token),
            None,
        )
        .await
        .unwrap();
    alice_ws
        .expect(|b| match b {
            S::PresenceChanged(p) if p.user_id == bob_id && p.online => Some(()),
            _ => None,
        })
        .await;
    assert!(
        api.send(&alice.token, &cid, "a-3", "hi again")
            .await
            .is_ok()
    );
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missed_events_are_recovered_from_the_change_feed() {
    let Some(db) = database() else { return };
    let srv = Server::start(&db, |_| {}).await;
    let api = srv.api();
    let alice = api.register("alice").await;
    let bob = api.register("bob").await;
    let cid = api
        .direct(&alice.token, &bob.user.clone().unwrap().user_id)
        .await
        .conversation_id;
    let first = api.send(&alice.token, &cid, "n0", "seen").await.unwrap();
    let applied = first.change_seq; // bob's cache is up to here

    // Bob is offline while alice sends 300, edits one and deletes another.
    let mut sent = Vec::new();
    for i in 1..=300 {
        sent.push(
            api.send(&alice.token, &cid, &format!("n{i}"), &format!("m{i}"))
                .await
                .unwrap(),
        );
    }
    let _: v1::ChatMessage = api
        .call(
            Method::PATCH,
            &format!("/v1/conversations/{cid}/messages/{}", sent[9].message_id),
            Some(&alice.token),
            Some(&v1::EditMessageRequest {
                body: "m10 edited".into(),
            }),
        )
        .await
        .unwrap();
    let _: v1::ChatMessage = api
        .call(
            Method::DELETE,
            &format!("/v1/conversations/{cid}/messages/{}", sent[19].message_id),
            Some(&alice.token),
            None,
        )
        .await
        .unwrap();

    // Bob resyncs from his cursor, page by page: every message exactly once, in change
    // order, the edit and the tombstone in their latest state.
    let mut cursor = applied;
    let mut got = Vec::new();
    loop {
        let page = api.changes(&bob.token, &cid, cursor).await;
        for m in &page.messages {
            assert!(m.change_seq > cursor, "ascending, no repeats");
            cursor = m.change_seq;
        }
        got.extend(page.messages);
        if !page.has_more {
            assert_eq!(cursor, page.change_seq);
            break;
        }
    }
    assert_eq!(
        got.len(),
        300,
        "300 messages, edits folded into their latest state"
    );
    let mut seqs: Vec<i64> = got.iter().map(|m| m.server_sequence).collect();
    seqs.sort_unstable();
    assert_eq!(seqs, (2..=301).collect::<Vec<_>>());
    let m10 = got
        .iter()
        .find(|m| m.message_id == sent[9].message_id)
        .unwrap();
    assert_eq!(m10.body, "m10 edited");
    let m20 = got
        .iter()
        .find(|m| m.message_id == sent[19].message_id)
        .unwrap();
    assert!(m20.deleted && m20.body.is_empty());
    // Nothing new after the cursor.
    assert!(
        api.changes(&bob.token, &cid, cursor)
            .await
            .messages
            .is_empty()
    );

    // History pages backwards with a cursor, no overlap.
    let mut before = 0;
    let mut all = Vec::new();
    loop {
        let path = format!("/v1/conversations/{cid}/messages?before={before}&limit=100");
        let page: v1::MessagePage = api
            .call(Method::GET, &path, Some(&bob.token), None)
            .await
            .unwrap();
        before = page.messages.last().map_or(0, |m| m.server_sequence);
        all.extend(page.messages.iter().map(|m| m.server_sequence));
        if !page.has_more {
            break;
        }
    }
    assert_eq!(all, (1..=301).rev().collect::<Vec<_>>());
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_senders_get_gap_free_order() {
    let Some(db) = database() else { return };
    let srv = Server::start(&db, |_| {}).await;
    let api = srv.api();
    let alice = api.register("alice").await;
    let bob = api.register("bob").await;
    let cid = api
        .direct(&alice.token, &bob.user.clone().unwrap().user_id)
        .await
        .conversation_id;
    let mut tasks = Vec::new();
    for (who, token) in [("a", alice.token.clone()), ("b", bob.token.clone())] {
        for i in 0..40 {
            let (api, token, cid) = (api.clone(), token.clone(), cid.clone());
            tasks.push(tokio::spawn(async move {
                let nonce = format!("{who}-{i}");
                // Every message is sent twice at once: retries must not duplicate it.
                let (x, y) = tokio::join!(
                    api.send(&token, &cid, &nonce, "race"),
                    api.send(&token, &cid, &nonce, "race")
                );
                let (x, y) = (x.unwrap(), y.unwrap());
                assert_eq!(x.message_id, y.message_id);
                x
            }));
        }
    }
    let mut seqs = Vec::new();
    let mut changes = Vec::new();
    for t in tasks {
        let m = t.await.unwrap();
        seqs.push(m.server_sequence);
        changes.push(m.change_seq);
    }
    seqs.sort_unstable();
    changes.sort_unstable();
    assert_eq!(seqs, (1..=80).collect::<Vec<_>>(), "unique and gap-free");
    assert_eq!(changes, (1..=80).collect::<Vec<_>>());
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn presence_follows_live_connections() {
    let Some(db) = database() else { return };
    let srv = Server::start(&db, |c| c.heartbeat_timeout = Duration::from_millis(1500)).await;
    let api = srv.api();
    let alice = api.register("alice").await;
    let bob = api.register("bob").await;
    let bob_id = bob.user.clone().unwrap().user_id;
    api.direct(&alice.token, &bob_id).await;
    let mut alice_ws = Live::connect(srv.addr, &alice.token).await.unwrap();

    let is_bob = |online: bool| {
        let bob_id = bob_id.clone();
        move |b: S| match b {
            S::PresenceChanged(p) if p.user_id == bob_id && p.online == online => Some(()),
            _ => None,
        }
    };
    // Two devices: online with the first, offline only after the last one closes.
    let bob1 = Live::connect(srv.addr, &bob.token).await.unwrap();
    alice_ws.expect(is_bob(true)).await;
    let second = api.login("bob", "correct horse").await.unwrap();
    let bob2 = Live::connect(srv.addr, &second.token).await.unwrap();
    drop(bob1);
    alice_ws
        .quiet(Duration::from_millis(400), |b| match b {
            S::PresenceChanged(p) if !p.online => Some(()),
            _ => None,
        })
        .await;
    assert!(
        api.conversations(&alice.token).await[0]
            .members
            .iter()
            .any(|m| m.user.as_ref().unwrap().user_id == bob_id && m.user.as_ref().unwrap().online)
    );
    // A device that stops sending (crash, cut cable) is offline after the heartbeat
    // timeout, without a close frame: no false "online".
    let silent = bob2;
    alice_ws.expect(is_bob(false)).await;
    drop(silent);
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authn_and_input_negatives() {
    let Some(db) = database() else { return };
    let srv = Server::start(&db, |c| c.anonymous_calls = false).await;
    let api = srv.api();
    let alice = api.register("alice").await;

    // Every authenticated endpoint refuses a missing / garbage / overlong token.
    let long = "x".repeat(500);
    for token in [None, Some("garbage"), Some(long.as_str())] {
        for (method, path) in [
            (Method::GET, "/v1/me"),
            (Method::GET, "/v1/conversations"),
            (Method::GET, "/v1/sessions"),
            (Method::GET, "/v1/blocks"),
            (Method::GET, "/v1/users/lookup?username=alice"),
            (Method::POST, "/v1/auth/refresh"),
            (Method::POST, "/v1/auth/logout"),
        ] {
            let (status, bytes) = api.raw(method, path, token, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
            let e = v1::ApiError::decode(bytes.as_slice()).unwrap();
            assert_eq!(e.code, ApiErrorCode::Unauthenticated as i32);
        }
    }
    // Wrong content type, malformed protobuf, oversized body.
    let (status, _) = api
        .http
        .post(api.url("/v1/auth/login"))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .map(|r| (r.status(), ()))
        .unwrap();
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = api
        .raw(
            Method::POST,
            "/v1/auth/login",
            None,
            Some(vec![0xFF, 0xFF, 0xFF]),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = api
        .raw(
            Method::POST,
            "/v1/auth/login",
            None,
            Some(vec![0; 70 * 1024]),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    // Malformed / unknown ids are 404, never 500.
    for path in [
        "/v1/conversations/not-a-uuid/messages",
        "/v1/conversations/00000000-0000-4000-8000-000000000000/messages",
    ] {
        let (status, _) = api.raw(Method::GET, path, Some(&alice.token), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    // Bad query numbers.
    let me = alice.user.clone().unwrap().user_id;
    let bob = api.register("bob").await;
    let cid = api.direct(&bob.token, &me).await.conversation_id;
    let (status, _) = api
        .raw(
            Method::GET,
            &format!("/v1/conversations/{cid}/changes?after=abc"),
            Some(&alice.token),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Conversation with yourself.
    let (status, _) = api
        .raw(
            Method::POST,
            "/v1/conversations/direct",
            Some(&alice.token),
            Some(v1::CreateDirectRequest { user_id: me }.encode_to_vec()),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // WebSocket: anonymous Hello refused on this server, bad token refused.
    let e = Live::connect(srv.addr, "").await.err().unwrap();
    assert_eq!(e.code, ErrorCode::Unauthenticated as i32);
    let e = Live::connect(srv.addr, "not a token").await.err().unwrap();
    assert_eq!(e.code, ErrorCode::Unauthenticated as i32);
    let live = Live::connect(srv.addr, &alice.token).await.unwrap();
    assert_eq!(live.welcome.user.unwrap().username, "alice");
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rate_limits_answer_429() {
    let Some(db) = database() else { return };
    let srv = Server::start(&db, |c| {
        c.limits.register_per_ip = Rate::new(2, 1);
        c.limits.login_per_ip = Rate::new(3, 1);
        c.limits.message_writes_per_user = Rate::new(5, 1);
    })
    .await;
    let api = srv.api();
    let a = api.register("alice").await;
    let b = api.register("bob").await;
    let e = api
        .call::<v1::AuthResponse>(
            Method::POST,
            "/v1/auth/register",
            None,
            Some(&v1::RegisterRequest {
                username: "carol".into(),
                password: "correct horse".into(),
                ..Default::default()
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(e.0, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(e.1.code, ApiErrorCode::RateLimited as i32);
    for _ in 0..3 {
        let _ = api.login("alice", "wrong guess!").await;
    }
    assert_eq!(
        api.login("alice", "correct horse").await.unwrap_err().0,
        StatusCode::TOO_MANY_REQUESTS,
        "password guessing is throttled per address"
    );
    let cid = api
        .direct(&a.token, &b.user.unwrap().user_id)
        .await
        .conversation_id;
    for i in 0..5 {
        api.send(&a.token, &cid, &format!("n{i}"), "spam")
            .await
            .unwrap();
    }
    assert_eq!(
        api.send(&a.token, &cid, "n5", "spam").await.unwrap_err().0,
        StatusCode::TOO_MANY_REQUESTS
    );
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn history_tokens_and_offline_state_survive_a_server_restart() {
    let Some(db) = database() else { return };
    let srv = Server::start(&db, |_| {}).await;
    let api = srv.api();
    let alice = api.register("alice").await;
    let bob = api.register("bob").await;
    let cid = api
        .direct(&alice.token, &bob.user.clone().unwrap().user_id)
        .await
        .conversation_id;
    for i in 0..5 {
        api.send(&alice.token, &cid, &format!("n{i}"), &format!("m{i}"))
            .await
            .unwrap();
    }
    let mut bob_ws = Live::connect(srv.addr, &bob.token).await.unwrap();
    srv.stop().await;
    assert!(
        bob_ws.next().await.is_none(),
        "connection ends with the server"
    );

    // Same database, new process state: migrations are a no-op, sessions still valid,
    // nobody is online, the conversation continues where it was.
    let srv = Server::start(&db, |_| {}).await;
    let api = srv.api();
    let page = api.changes(&bob.token, &cid, 0).await;
    assert_eq!(page.messages.len(), 5);
    let conv = &api.conversations(&alice.token).await[0];
    assert!(conv.members.iter().all(|m| {
        let u = m.user.as_ref().unwrap();
        u.user_id == alice.user.as_ref().unwrap().user_id || !u.online
    }));
    let next = api
        .send(&alice.token, &cid, "after-restart", "still here")
        .await
        .unwrap();
    assert_eq!(next.server_sequence, 6);
    // A nonce from before the restart is still recognised.
    assert_eq!(
        api.send(&alice.token, &cid, "n0", "m0")
            .await
            .unwrap()
            .server_sequence,
        1
    );
    srv.stop().await;
}

#[tokio::test]
async fn messenger_without_database_answers_503() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = Config {
        media_udp: false,
        ..Default::default()
    };
    tokio::spawn(turbo_pancake_app_server::serve(
        listener,
        config,
        std::future::pending(),
    ));
    let api = Api::new(addr);
    let mut status = None;
    for _ in 0..100 {
        if let Ok(r) = api.http.post(api.url("/v1/auth/login")).send().await {
            status = Some(r.status());
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Content type is checked after the database extractor.
    let e = api.login("alice", "correct horse").await.unwrap_err();
    assert_eq!(e.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(e.1.code, ApiErrorCode::Unavailable as i32);
    assert!(status.is_some());
}
