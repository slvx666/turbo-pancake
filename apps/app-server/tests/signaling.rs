//! Integration tests: real TCP + WebSocket against an in-process server.

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use prost::Message as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use turbo_pancake_app_server::Config;
use turbo_pancake_protocol::v1::{
    self, ErrorCode, LeaveReason, client_envelope::Body as C, server_envelope::Body as S,
};
use turbo_pancake_protocol::{PROTOCOL_VERSION, client, decode_server, encode};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const STEP: Duration = Duration::from_secs(5);

async fn start(config: Config) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(turbo_pancake_app_server::serve(
        listener,
        config,
        std::future::pending(),
    ));
    addr
}

struct Client {
    ws: Ws,
    next_rid: u64,
    last_event: u64,
    peer_id: String,
}

impl Client {
    async fn connect(addr: SocketAddr) -> Self {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/v1/signal"))
            .await
            .unwrap();
        Self {
            ws,
            next_rid: 1,
            last_event: 0,
            peer_id: String::new(),
        }
    }

    async fn hello(addr: SocketAddr, name: &str) -> Self {
        let mut c = Self::connect(addr).await;
        let rid = c
            .send(C::Hello(v1::Hello {
                client_version: "test/0".into(),
                display_name: name.into(),
                ..Default::default()
            }))
            .await;
        let (r, body) = c.recv().await;
        assert_eq!(r, rid);
        let S::Welcome(w) = body else {
            panic!("expected Welcome, got {body:?}")
        };
        assert!(!w.peer_id.is_empty());
        c.peer_id = w.peer_id;
        c
    }

    async fn send(&mut self, body: C) -> u64 {
        let rid = self.next_rid;
        self.next_rid += 1;
        self.send_raw(encode(&client(rid, body))).await;
        rid
    }

    async fn send_raw(&mut self, bytes: Vec<u8>) {
        self.ws.send(Message::Binary(bytes.into())).await.unwrap();
    }

    /// Next server envelope; asserts event ids are gap-free and monotonic.
    async fn recv(&mut self) -> (u64, S) {
        loop {
            let msg = tokio::time::timeout(STEP, self.ws.next())
                .await
                .expect("timed out waiting for server frame")
                .expect("stream ended")
                .expect("ws error");
            match msg {
                Message::Binary(b) => {
                    let env = decode_server(&b).unwrap();
                    assert_eq!(env.event_id, self.last_event + 1, "event id gap");
                    self.last_event = env.event_id;
                    return (env.request_id, env.body.unwrap());
                }
                Message::Ping(_) | Message::Pong(_) => continue,
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }

    async fn expect_error(&mut self, rid: u64, code: ErrorCode) {
        let (r, body) = self.recv().await;
        assert_eq!(r, rid);
        match body {
            S::Error(e) => assert_eq!(e.code(), code, "{}", e.message),
            other => panic!("expected error {code:?}, got {other:?}"),
        }
    }

    async fn join(&mut self, room: &str) -> Vec<v1::Peer> {
        let rid = self
            .send(C::JoinRoom(v1::JoinRoom {
                room_id: room.into(),
            }))
            .await;
        let (r, body) = self.recv().await;
        assert_eq!(r, rid);
        let S::RoomJoined(j) = body else {
            panic!("expected RoomJoined, got {body:?}")
        };
        assert_eq!(j.room_id, room);
        j.peers
    }

    async fn expect_closed(&mut self) {
        loop {
            match tokio::time::timeout(STEP, self.ws.next())
                .await
                .expect("socket not closed in time")
            {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                Some(Ok(_)) => continue,
            }
        }
    }
}

fn offer(room: &str, to: &str, sdp: &str) -> C {
    C::Signal(v1::Signal {
        room_id: room.into(),
        to_peer_id: to.into(),
        payload: Some(v1::signal::Payload::Description(v1::SessionDescription {
            sdp_type: v1::SdpType::Offer as i32,
            sdp: sdp.into(),
        })),
    })
}

#[tokio::test]
async fn two_peers_join_and_exchange_offer_answer_ice() {
    let addr = start(Config::default()).await;
    let mut a = Client::hello(addr, "Алиса").await;
    let mut b = Client::hello(addr, "Bob").await;
    assert_ne!(a.peer_id, b.peer_id);

    assert!(a.join("call-1").await.is_empty());
    let peers = b.join("call-1").await;
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].peer_id, a.peer_id);
    assert_eq!(peers[0].display_name, "Алиса");

    let (r, body) = a.recv().await;
    assert_eq!(r, 0);
    let S::PeerJoined(pj) = body else {
        panic!("{body:?}")
    };
    assert_eq!(pj.peer.unwrap().peer_id, b.peer_id);

    // offer a -> b
    a.send(offer("call-1", &b.peer_id, "v=0 offer")).await;
    let (_, body) = b.recv().await;
    let S::Signal(sig) = body else {
        panic!("{body:?}")
    };
    assert_eq!(sig.from_peer_id, a.peer_id);
    let Some(v1::signal_relay::Payload::Description(d)) = sig.payload else {
        panic!()
    };
    assert_eq!(d.sdp_type(), v1::SdpType::Offer);
    assert_eq!(d.sdp, "v=0 offer");

    // answer + ice b -> a
    b.send(C::Signal(v1::Signal {
        room_id: "call-1".into(),
        to_peer_id: a.peer_id.clone(),
        payload: Some(v1::signal::Payload::Description(v1::SessionDescription {
            sdp_type: v1::SdpType::Answer as i32,
            sdp: "v=0 answer".into(),
        })),
    }))
    .await;
    b.send(C::Signal(v1::Signal {
        room_id: "call-1".into(),
        to_peer_id: a.peer_id.clone(),
        payload: Some(v1::signal::Payload::IceCandidate(v1::IceCandidate {
            candidate: "candidate:1 1 udp 2122260223 127.0.0.1 50000 typ host".into(),
            sdp_mid: "0".into(),
            sdp_mline_index: 0,
        })),
    }))
    .await;
    let (_, body) = a.recv().await;
    assert!(matches!(
        body,
        S::Signal(v1::SignalRelay { payload: Some(v1::signal_relay::Payload::Description(ref d)), .. })
            if d.sdp_type() == v1::SdpType::Answer
    ));
    let (_, body) = a.recv().await;
    assert!(matches!(
        body,
        S::Signal(v1::SignalRelay {
            payload: Some(v1::signal_relay::Payload::IceCandidate(_)),
            ..
        })
    ));
}

#[tokio::test]
async fn requests_before_hello_are_rejected() {
    let addr = start(Config::default()).await;
    let mut c = Client::connect(addr).await;
    let rid = c
        .send(C::JoinRoom(v1::JoinRoom {
            room_id: "r".into(),
        }))
        .await;
    c.expect_error(rid, ErrorCode::HelloRequired).await;
}

#[tokio::test]
async fn invalid_hello_and_duplicate_hello() {
    let addr = start(Config::default()).await;
    let mut c = Client::connect(addr).await;
    let rid = c
        .send(C::Hello(v1::Hello {
            client_version: String::new(),
            display_name: "  ".into(),
            ..Default::default()
        }))
        .await;
    c.expect_error(rid, ErrorCode::BadRequest).await;

    let mut ok = Client::hello(addr, "x").await;
    let rid = ok
        .send(C::Hello(v1::Hello {
            client_version: String::new(),
            display_name: "y".into(),
            ..Default::default()
        }))
        .await;
    ok.expect_error(rid, ErrorCode::BadRequest).await;
}

#[tokio::test]
async fn unsupported_version_gets_error_and_close() {
    let addr = start(Config::default()).await;
    let mut c = Client::connect(addr).await;
    let mut env = client(9, C::Ping(v1::Ping::default()));
    env.protocol_version = PROTOCOL_VERSION + 1;
    c.send_raw(env.encode_to_vec()).await;
    c.expect_error(0, ErrorCode::UnsupportedVersion).await;
    c.expect_closed().await;
}

#[tokio::test]
async fn malformed_and_text_frames_are_rejected_without_crash() {
    let addr = start(Config::default()).await;
    let mut c = Client::hello(addr, "x").await;
    c.send_raw(vec![0xFF, 0xFF, 0xFF]).await;
    c.expect_error(0, ErrorCode::BadRequest).await;
    c.ws.send(Message::Text("hi".into())).await.unwrap();
    c.expect_error(0, ErrorCode::BadRequest).await;
    // still usable
    let rid = c.send(C::Ping(v1::Ping { client_time_us: 5 })).await;
    let (r, body) = c.recv().await;
    assert_eq!(r, rid);
    assert!(matches!(body, S::Pong(v1::Pong { client_time_us: 5 })));
}

#[tokio::test]
async fn oversized_frame_closes_connection() {
    let addr = start(Config::default()).await;
    let mut c = Client::hello(addr, "x").await;
    let _ =
        c.ws.send(Message::Binary(
            vec![0u8; turbo_pancake_protocol::MAX_FRAME_BYTES + 1].into(),
        ))
        .await;
    c.expect_closed().await;
}

#[tokio::test]
async fn signal_routing_errors() {
    let addr = start(Config::default()).await;
    let mut a = Client::hello(addr, "a").await;
    let mut b = Client::hello(addr, "b").await;

    // not in any room
    let rid = a.send(offer("r1", &b.peer_id, "x")).await;
    a.expect_error(rid, ErrorCode::NotInRoom).await;

    a.join("r1").await;
    b.join("r2").await;
    // b exists but is in another room
    let rid = a.send(offer("r1", &b.peer_id, "x")).await;
    a.expect_error(rid, ErrorCode::PeerNotFound).await;
    // claims a room it is not in
    let rid = a.send(offer("r2", &b.peer_id, "x")).await;
    a.expect_error(rid, ErrorCode::NotInRoom).await;
    // self
    let me = a.peer_id.clone();
    let rid = a.send(offer("r1", &me, "x")).await;
    a.expect_error(rid, ErrorCode::BadRequest).await;
    // invalid room id
    let rid = a
        .send(C::JoinRoom(v1::JoinRoom {
            room_id: "bad room!".into(),
        }))
        .await;
    a.expect_error(rid, ErrorCode::BadRequest).await;
}

/// One active call per connection: joining another room closes the current one; join and
/// leave are idempotent.
#[tokio::test]
async fn joining_another_room_closes_the_previous_call() {
    let addr = start(Config::default()).await;
    let mut a = Client::hello(addr, "a").await;
    let mut b = Client::hello(addr, "b").await;
    let mut c = Client::hello(addr, "c").await;
    a.join("r1").await;
    b.join("r1").await;
    assert!(matches!(a.recv().await.1, S::PeerJoined(_)));
    c.join("r2").await;

    // Repeated join of the same room: the members again, nobody else hears about it.
    let peers = a.join("r1").await;
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].peer_id, b.peer_id);

    // a joins r2: RoomLeft{r1} first, then RoomJoined{r2}; b sees a leave (SWITCHED),
    // c sees a arrive.
    let rid = a
        .send(C::JoinRoom(v1::JoinRoom {
            room_id: "r2".into(),
        }))
        .await;
    let (r, body) = a.recv().await;
    assert_eq!(r, 0);
    let S::RoomLeft(left) = body else {
        panic!("expected RoomLeft, got {body:?}")
    };
    assert_eq!(left.room_id, "r1");
    let (r, body) = a.recv().await;
    assert_eq!(r, rid);
    let S::RoomJoined(j) = body else {
        panic!("expected RoomJoined, got {body:?}")
    };
    assert_eq!(j.room_id, "r2");
    assert_eq!(j.peers.len(), 1);
    assert_eq!(j.peers[0].peer_id, c.peer_id);
    let S::PeerLeft(pl) = b.recv().await.1 else {
        panic!()
    };
    assert_eq!(pl.peer_id, a.peer_id);
    assert_eq!(pl.reason(), LeaveReason::Switched);
    let S::PeerJoined(pj) = c.recv().await.1 else {
        panic!()
    };
    assert_eq!(pj.peer.unwrap().peer_id, a.peer_id);

    // a is no longer in r1: signaling there is refused.
    let rid = a.send(offer("r1", &b.peer_id, "x")).await;
    a.expect_error(rid, ErrorCode::NotInRoom).await;

    // Leaving a room we are not in only confirms it (a stays in r2).
    let rid = a
        .send(C::LeaveRoom(v1::LeaveRoom {
            room_id: "r1".into(),
        }))
        .await;
    let (r, body) = a.recv().await;
    assert_eq!(r, rid);
    assert!(matches!(body, S::RoomLeft(ref l) if l.room_id == "r1"));
    a.send(offer("r2", &c.peer_id, "x")).await;
    let S::Signal(_) = c.recv().await.1 else {
        panic!()
    };
}

/// A full room refuses the switch and the current call goes on.
#[tokio::test]
async fn switch_to_a_full_room_keeps_the_current_call() {
    let addr = start(Config {
        max_room_peers: 1,
        ..Config::default()
    })
    .await;
    let mut a = Client::hello(addr, "a").await;
    let mut b = Client::hello(addr, "b").await;
    a.join("r1").await;
    b.join("r2").await;
    let rid = a
        .send(C::JoinRoom(v1::JoinRoom {
            room_id: "r2".into(),
        }))
        .await;
    a.expect_error(rid, ErrorCode::RoomFull).await;
    // Still in r1: a repeated join answers with r1's members.
    assert!(a.join("r1").await.is_empty());
}

#[tokio::test]
async fn leave_and_disconnect_notify_others() {
    let addr = start(Config::default()).await;
    let mut a = Client::hello(addr, "a").await;
    let mut b = Client::hello(addr, "b").await;
    let mut c = Client::hello(addr, "c").await;
    a.join("room").await;
    b.join("room").await;
    c.join("room").await;
    // drain PeerJoined events
    for _ in 0..2 {
        assert!(matches!(a.recv().await.1, S::PeerJoined(_)));
    }
    assert!(matches!(b.recv().await.1, S::PeerJoined(_)));

    // explicit leave
    let rid = b
        .send(C::LeaveRoom(v1::LeaveRoom {
            room_id: "room".into(),
        }))
        .await;
    let (r, body) = b.recv().await;
    assert_eq!(r, rid);
    assert!(matches!(body, S::RoomLeft(_)));
    for x in [&mut a, &mut c] {
        let S::PeerLeft(pl) = x.recv().await.1 else {
            panic!()
        };
        assert_eq!(pl.peer_id, b.peer_id);
        assert_eq!(pl.reason(), LeaveReason::Left);
    }

    // abrupt disconnect (drop the socket without close handshake)
    let c_id = c.peer_id.clone();
    drop(c);
    let S::PeerLeft(pl) = a.recv().await.1 else {
        panic!()
    };
    assert_eq!(pl.peer_id, c_id);
    assert_eq!(pl.reason(), LeaveReason::Disconnected);

    // b can rejoin after leaving (idempotent lifecycle)
    let peers = b.join("room").await;
    assert_eq!(peers.len(), 1);
}

#[tokio::test]
async fn heartbeat_timeout_evicts_silent_peer() {
    let addr = start(Config {
        heartbeat_timeout: Duration::from_millis(400),
        heartbeat_interval: Duration::from_millis(100),
        ..Config::default()
    })
    .await;
    let mut a = Client::hello(addr, "a").await;
    let mut b = Client::hello(addr, "b").await;
    a.join("hb").await;
    b.join("hb").await;
    assert!(matches!(a.recv().await.1, S::PeerJoined(_)));

    // a keeps pinging, b stays silent
    let b_id = b.peer_id.clone();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let left = loop {
        assert!(tokio::time::Instant::now() < deadline, "b was not evicted");
        a.send(C::Ping(v1::Ping::default())).await;
        match tokio::time::timeout(Duration::from_millis(150), a.recv()).await {
            Ok((_, S::PeerLeft(pl))) => break pl,
            Ok((_, S::Pong(_))) | Err(_) => continue,
            Ok((_, other)) => panic!("{other:?}"),
        }
    };
    assert_eq!(left.peer_id, b_id);
    assert_eq!(left.reason(), LeaveReason::Timeout);
    b.expect_closed().await;
}

#[tokio::test]
async fn room_capacity_is_enforced() {
    let addr = start(Config {
        max_room_peers: 2,
        ..Config::default()
    })
    .await;
    let mut a = Client::hello(addr, "a").await;
    let mut b = Client::hello(addr, "b").await;
    let mut c = Client::hello(addr, "c").await;
    a.join("small").await;
    b.join("small").await;
    let rid = c
        .send(C::JoinRoom(v1::JoinRoom {
            room_id: "small".into(),
        }))
        .await;
    c.expect_error(rid, ErrorCode::RoomFull).await;
}

#[tokio::test]
async fn twenty_peers_join_and_leave() {
    let addr = start(Config::default()).await;
    let mut clients = Vec::new();
    for i in 0..20 {
        let mut c = Client::hello(addr, &format!("peer {i}")).await;
        let peers = c.join("big").await;
        assert_eq!(peers.len(), i);
        clients.push(c);
    }
    // everyone except the last saw (20 - 1 - index) PeerJoined events
    for (i, c) in clients.iter_mut().enumerate() {
        for _ in 0..(19 - i) {
            assert!(matches!(c.recv().await.1, S::PeerJoined(_)));
        }
    }
    // last peer offers to the first; first receives it
    let first = clients[0].peer_id.clone();
    clients[19].send(offer("big", &first, "v=0")).await;
    assert!(matches!(clients[0].recv().await.1, S::Signal(_)));

    // all leave; remaining peers see PeerLeft each time
    while let Some(c) = clients.pop() {
        drop(c);
        for other in clients.iter_mut() {
            assert!(matches!(other.recv().await.1, S::PeerLeft(_)));
        }
    }
}

async fn http_get(addr: SocketAddr, path: &str) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    out
}

#[tokio::test]
async fn health_ready_version_endpoints() {
    let addr = start(Config::default()).await;
    let h = http_get(addr, "/healthz").await;
    assert!(h.starts_with("HTTP/1.1 200"), "{h}");
    assert!(h.ends_with("ok"));
    let r = http_get(addr, "/readyz").await;
    assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    let v = http_get(addr, "/v1/version").await;
    assert!(v.contains("\"protocol_version\":1"), "{v}");
    let m = http_get(addr, "/v1/media-stats").await;
    assert!(m.starts_with("HTTP/1.1 200"), "{m}");
    assert!(
        m.contains("\"rtp_in\":0") && m.contains("\"udp_send_errors\":0"),
        "{m}"
    );
}

#[tokio::test]
async fn connection_limit_rejects_excess_upgrades() {
    let addr = start(Config {
        max_connections: 1,
        ..Config::default()
    })
    .await;
    let _a = Client::hello(addr, "a").await;
    let err = tokio_tungstenite::connect_async(format!("ws://{addr}/v1/signal"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("503"), "{err}");
}
