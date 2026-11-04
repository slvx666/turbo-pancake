//! Two messenger clients (the real `Service`: engine + cache + WebSocket) against a real
//! app-server and PostgreSQL: exchange, receipts, offline sends, server and client
//! restarts, missed events, blocks, revoked sessions (S5 acceptance, headless part).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::mpsc;
use turbo_pancake_app_server::Config;
use turbo_pancake_app_server::ratelimit::Limits;
use turbo_pancake_messenger_client::{
    Command, ConversationView, MessageState, MessageView, Service, ServiceConfig, Status, Update,
};
use turbo_pancake_protocol::v1;

const WAIT: Duration = Duration::from_secs(15);

struct Server {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

async fn serve(db: &str, addr: Option<SocketAddr>) -> (SocketAddr, Server) {
    let config = Config {
        database_url: Some(db.to_owned()),
        database_pool: 8,
        limits: Limits::relaxed(),
        media_udp: false,
        ..Default::default()
    };
    let listener = match addr {
        // Restart on the same port (clients reconnect to the address they know).
        Some(a) => loop {
            match tokio::net::TcpListener::bind(a).await {
                Ok(l) => break l,
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        },
        None => tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
    };
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(turbo_pancake_app_server::serve(listener, config, async {
        let _ = rx.await;
    }));
    (
        addr,
        Server {
            stop: Some(tx),
            task,
        },
    )
}

impl Server {
    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        let _ = tokio::time::timeout(WAIT, self.task).await;
    }
}

/// A client and the latest state its updates described.
struct Client {
    service: Option<Service>,
    rx: mpsc::UnboundedReceiver<Update>,
    status: Status,
    user: Option<v1::User>,
    token: String,
    signed_out: Option<String>,
    conversations: Vec<ConversationView>,
    messages: HashMap<String, Vec<MessageView>>,
    opened: Option<String>,
    errors: Vec<String>,
}

fn cache_dir(name: &str) -> PathBuf {
    // In the build directory (drive with space), unique per test process.
    let exe = std::env::current_exe().unwrap();
    exe.parent()
        .unwrap()
        .join(format!("tp-client-{}", std::process::id()))
        .join(name)
}

impl Client {
    fn start(addr: SocketAddr, cache: &str) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let service = Service::start(
            ServiceConfig {
                server: format!("http://{addr}"),
                device_name: format!("test {cache}"),
                client_version: "test".into(),
                cache_dir: Some(cache_dir(cache)),
                view_limit: 500,
                tls: None,
            },
            move |u| {
                let _ = tx.send(u);
            },
        );
        Self {
            service: Some(service),
            rx,
            status: Status::SignedOut,
            user: None,
            token: String::new(),
            signed_out: None,
            conversations: Vec::new(),
            messages: HashMap::new(),
            opened: None,
            errors: Vec::new(),
        }
    }

    fn cmd(&self, c: Command) {
        self.service.as_ref().unwrap().send(c);
    }

    fn apply(&mut self, u: Update) {
        match u {
            Update::Status(s) => self.status = s,
            Update::SignedIn { user, token } => {
                self.user = Some(user);
                self.token = token;
                self.signed_out = None;
            }
            Update::SignedOut { reason } => self.signed_out = Some(reason),
            Update::Conversations(c) => self.conversations = c,
            Update::Messages {
                conversation_id,
                messages,
            } => {
                self.messages.insert(conversation_id, messages);
            }
            Update::Opened { conversation_id } => self.opened = Some(conversation_id),
            Update::Error(e) => self.errors.push(e),
        }
    }

    /// Applies updates until `pred` holds.
    async fn until(&mut self, what: &str, pred: impl Fn(&Client) -> bool) {
        let end = tokio::time::Instant::now() + WAIT;
        while !pred(self) {
            match tokio::time::timeout_at(end, self.rx.recv()).await {
                Ok(Some(u)) => self.apply(u),
                _ => panic!(
                    "timed out waiting for {what}; status {:?}, errors {:?}",
                    self.status, self.errors
                ),
            }
        }
    }

    async fn sign_up(&mut self, name: &str) {
        self.cmd(Command::Register {
            username: name.into(),
            password: "correct horse".into(),
            display_name: name.to_uppercase(),
        });
        self.until("online", |c| c.status == Status::Online).await;
    }

    fn msgs(&self, conv: &str) -> &[MessageView] {
        self.messages.get(conv).map(Vec::as_slice).unwrap_or(&[])
    }

    fn bodies(&self, conv: &str) -> Vec<String> {
        self.msgs(conv).iter().map(|m| m.body.clone()).collect()
    }

    fn conv(&self, id: &str) -> Option<&ConversationView> {
        self.conversations.iter().find(|c| c.id == id)
    }

    async fn stop(&mut self) {
        let s = self.service.take();
        tokio::task::spawn_blocking(move || drop(s)).await.unwrap();
    }
}

fn database() -> Option<String> {
    let url = turbo_pancake_pg_test::database_url();
    if url.is_none() {
        eprintln!("skipped: no PostgreSQL");
    }
    url
}

/// alice and bob signed in with a direct conversation open on alice's side.
async fn pair(addr: SocketAddr, tag: &str) -> (Client, Client, String) {
    let mut alice = Client::start(addr, &format!("{tag}-alice"));
    let mut bob = Client::start(addr, &format!("{tag}-bob"));
    alice.sign_up("alice").await;
    bob.sign_up("bob").await;
    alice.cmd(Command::StartDirect {
        username: "@Bob".into(),
    });
    alice
        .until("conversation opened", |c| c.opened.is_some())
        .await;
    let conv = alice.opened.clone().unwrap();
    bob.until("conversation appears for bob", |c| c.conv(&conv).is_some())
        .await;
    (alice, bob, conv)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exchange_receipts_edit_delete_and_presence() {
    let Some(db) = database() else { return };
    let (addr, srv) = serve(&db, None).await;
    let (mut alice, mut bob, conv) = pair(addr, "exchange").await;
    alice
        .until("titled by the peer", |c| {
            c.conv(&conv).is_some_and(|v| v.title == "BOB")
        })
        .await;
    alice
        .until("bob online", |c| c.conv(&conv).is_some_and(|v| v.online))
        .await;

    // alice → bob; bob has not opened it: unread 1, alice sees "delivered".
    alice.cmd(Command::Send {
        conversation_id: conv.clone(),
        body: "привет 👋".into(),
        reply_to_id: String::new(),
    });
    bob.until("unread 1", |c| {
        c.conv(&conv)
            .is_some_and(|v| v.unread == 1 && v.last_text == "привет 👋")
    })
    .await;
    alice
        .until("delivered", |c| {
            c.msgs(&conv)
                .last()
                .is_some_and(|m| m.state == MessageState::Delivered)
        })
        .await;

    // bob opens it: read on both sides; replies.
    bob.cmd(Command::Open {
        conversation_id: Some(conv.clone()),
    });
    bob.until("unread 0", |c| c.conv(&conv).is_some_and(|v| v.unread == 0))
        .await;
    alice
        .until("read", |c| {
            c.msgs(&conv)
                .last()
                .is_some_and(|m| m.state == MessageState::Read)
        })
        .await;
    bob.until("bob's view", |c| c.msgs(&conv).len() == 1).await;
    let first_id = bob.msgs(&conv)[0].message_id.clone();
    bob.cmd(Command::Send {
        conversation_id: conv.clone(),
        body: "hi!".into(),
        reply_to_id: first_id.clone(),
    });
    alice
        .until("reply arrives", |c| c.msgs(&conv).len() == 2)
        .await;
    let reply = alice.msgs(&conv)[1].reply.clone().unwrap();
    assert_eq!(
        (reply.author_name.as_str(), reply.text.as_str()),
        ("ALICE", "привет 👋")
    );

    // Edit and delete for everyone reach bob's view (and cache).
    alice.cmd(Command::Edit {
        conversation_id: conv.clone(),
        message_id: first_id.clone(),
        body: "привет, Боб".into(),
    });
    bob.until("edit", |c| {
        c.msgs(&conv)
            .first()
            .is_some_and(|m| m.body == "привет, Боб" && m.edited)
    })
    .await;
    alice.cmd(Command::Delete {
        conversation_id: conv.clone(),
        message_id: first_id.clone(),
    });
    bob.until("delete", |c| {
        c.msgs(&conv)
            .first()
            .is_some_and(|m| m.deleted && m.body.is_empty())
    })
    .await;
    bob.until("reply quote shows deleted", |c| {
        c.msgs(&conv)
            .get(1)
            .and_then(|m| m.reply.clone())
            .is_some_and(|r| r.deleted)
    })
    .await;

    // bob closes the app: alice sees him offline.
    bob.stop().await;
    alice
        .until("bob offline", |c| c.conv(&conv).is_some_and(|v| !v.online))
        .await;
    alice.stop().await;
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offline_sends_server_restart_and_client_restart_without_duplicates() {
    let Some(db) = database() else { return };
    let (addr, srv) = serve(&db, None).await;
    let (mut alice, mut bob, conv) = pair(addr, "restart").await;
    for i in 0..3 {
        alice.cmd(Command::Send {
            conversation_id: conv.clone(),
            body: format!("before {i}"),
            reply_to_id: String::new(),
        });
    }
    bob.cmd(Command::Open {
        conversation_id: Some(conv.clone()),
    });
    bob.until("3 messages", |c| c.msgs(&conv).len() == 3).await;

    // Server goes down: both clients notice; alice keeps typing — her messages wait as
    // pending (optimistic UI), the draft-free input keeps working.
    srv.stop().await;
    alice
        .until("reconnecting", |c| {
            matches!(c.status, Status::Reconnecting { .. })
        })
        .await;
    alice.cmd(Command::Open {
        conversation_id: Some(conv.clone()),
    });
    for i in 0..2 {
        alice.cmd(Command::Send {
            conversation_id: conv.clone(),
            body: format!("offline {i}"),
            reply_to_id: String::new(),
        });
    }
    alice
        .until("2 pending", |c| {
            c.msgs(&conv)
                .iter()
                .filter(|m| m.state == MessageState::Pending)
                .count()
                == 2
        })
        .await;

    // bob closes his client while the server is down (his token stays stored).
    let (token, user) = (bob.token.clone(), bob.user.clone().unwrap());
    bob.stop().await;

    // Server back on the same address and database.
    let (_, srv) = serve(&db, Some(addr)).await;
    alice
        .until("online again", |c| c.status == Status::Online)
        .await;
    alice
        .until("pending sent", |c| {
            c.msgs(&conv).len() == 5 && c.msgs(&conv).iter().all(|m| m.server_sequence > 0)
        })
        .await;

    // bob restarts from his cache: history at once (from SQLite), then exactly the two
    // new messages, no duplicates, in server order.
    let mut bob = Client::start(addr, "restart-bob");
    bob.cmd(Command::Resume { token, user });
    bob.cmd(Command::Open {
        conversation_id: Some(conv.clone()),
    });
    bob.until("history from cache and server", |c| {
        c.msgs(&conv).len() == 5
    })
    .await;
    assert_eq!(
        bob.bodies(&conv),
        ["before 0", "before 1", "before 2", "offline 0", "offline 1"]
    );
    let seqs: Vec<i64> = bob.msgs(&conv).iter().map(|m| m.server_sequence).collect();
    assert_eq!(seqs, [1, 2, 3, 4, 5]);
    // Stays exactly 5 after more resyncs (status flips) and a reopen.
    bob.cmd(Command::Open {
        conversation_id: Some(conv.clone()),
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(u) = bob.rx.try_recv() {
        bob.apply(u);
    }
    assert_eq!(bob.msgs(&conv).len(), 5);
    alice.stop().await;
    bob.stop().await;
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_shows_the_cache_offline_and_catches_up() {
    let Some(db) = database() else { return };
    let (addr, srv) = serve(&db, None).await;
    let (mut alice, mut bob, conv) = pair(addr, "resume").await;
    bob.cmd(Command::Open {
        conversation_id: Some(conv.clone()),
    });
    alice.cmd(Command::Send {
        conversation_id: conv.clone(),
        body: "one".into(),
        reply_to_id: String::new(),
    });
    bob.until("one", |c| c.msgs(&conv).len() == 1).await;
    let (token, user) = (bob.token.clone(), bob.user.clone().unwrap());
    bob.stop().await;

    // While bob is away, alice sends 30 more (bob misses all their events).
    for i in 0..30 {
        alice.cmd(Command::Send {
            conversation_id: conv.clone(),
            body: format!("m{i}"),
            reply_to_id: String::new(),
        });
    }
    alice
        .until("all sent", |c| {
            c.msgs(&conv).len() == 31 && c.msgs(&conv).iter().all(|m| m.server_sequence > 0)
        })
        .await;

    // Server down, bob starts: his cache shows the conversation without the server.
    srv.stop().await;
    let mut bob = Client::start(addr, "resume-bob");
    bob.cmd(Command::Resume {
        token: token.clone(),
        user: user.clone(),
    });
    bob.cmd(Command::Open {
        conversation_id: Some(conv.clone()),
    });
    bob.until("cached history offline", |c| c.bodies(&conv) == ["one"])
        .await;
    assert_ne!(bob.status, Status::Online);

    // Server back: bob catches up from the change feed, exactly once.
    let (_, srv) = serve(&db, Some(addr)).await;
    bob.until("caught up", |c| c.msgs(&conv).len() == 31).await;
    let mut seqs: Vec<i64> = bob.msgs(&conv).iter().map(|m| m.server_sequence).collect();
    seqs.dedup();
    assert_eq!(seqs, (1..=31).collect::<Vec<_>>());
    alice
        .until("alice sees bob's read", |c| {
            c.msgs(&conv)
                .last()
                .is_some_and(|m| m.state == MessageState::Read)
        })
        .await;
    alice.stop().await;
    bob.stop().await;
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocked_send_fails_visibly_and_revoked_session_signs_out() {
    let Some(db) = database() else { return };
    let (addr, srv) = serve(&db, None).await;
    let (mut alice, mut bob, conv) = pair(addr, "block").await;
    let alice_id = alice.user.clone().unwrap().user_id;
    bob.cmd(Command::Block {
        user_id: alice_id.clone(),
        blocked: true,
    });
    bob.until("blocked flag", |c| {
        c.conv(&conv)
            .is_some_and(|v| v.blocked_by_me && !v.can_send)
    })
    .await;
    alice
        .until("cannot send", |c| {
            c.conv(&conv).is_some_and(|v| !v.can_send)
        })
        .await;
    alice.cmd(Command::Open {
        conversation_id: Some(conv.clone()),
    });
    alice.cmd(Command::Send {
        conversation_id: conv.clone(),
        body: "let me talk".into(),
        reply_to_id: String::new(),
    });
    alice
        .until("failed with reason", |c| {
            c.msgs(&conv).last().is_some_and(|m| {
                m.state == MessageState::Failed("you cannot message this user".into())
            })
        })
        .await;
    // Discard removes it; unblock lets a new one through.
    let nonce = alice.msgs(&conv).last().unwrap().key.clone();
    alice.cmd(Command::Discard { nonce });
    alice.until("discarded", |c| c.msgs(&conv).is_empty()).await;
    bob.cmd(Command::Block {
        user_id: alice_id,
        blocked: false,
    });
    alice
        .until("can send again", |c| {
            c.conv(&conv).is_some_and(|v| v.can_send)
        })
        .await;
    alice.cmd(Command::Send {
        conversation_id: conv.clone(),
        body: "thanks".into(),
        reply_to_id: String::new(),
    });
    alice
        .until("sent", |c| {
            c.msgs(&conv).last().is_some_and(|m| m.server_sequence == 1)
        })
        .await;

    // alice's session is ended from another device: her client signs out and the local
    // history file is removed.
    let api = turbo_pancake_messenger_client::api::Api::new(&format!("http://{addr}"));
    let other = api
        .login(&v1::LoginRequest {
            username: "alice".into(),
            password: "correct horse".into(),
            device_name: "other".into(),
        })
        .await
        .unwrap();
    let mut api = api;
    api.set_token(Some(other.token));
    let sessions = api.sessions().await.unwrap();
    let first = sessions.sessions.iter().find(|s| !s.current).unwrap();
    api.revoke_session(&first.session_id).await.unwrap();
    alice.until("signed out", |c| c.signed_out.is_some()).await;
    alice
        .until("status signed out", |c| c.status == Status::SignedOut)
        .await;
    let file = std::fs::read_dir(cache_dir("block-alice"))
        .unwrap()
        .flatten()
        .flat_map(|d| std::fs::read_dir(d.path()).unwrap().flatten())
        .filter(|f| f.path().extension().is_some_and(|e| e == "sqlite3"))
        .count();
    assert_eq!(file, 0, "cache wiped after the session ended");
    alice.stop().await;
    bob.stop().await;
    srv.stop().await;
}
