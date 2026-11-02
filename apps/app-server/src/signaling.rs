//! Development signaling relay: peers say `Hello`, join a room and exchange
//! SDP/ICE with other members of that room through the server.
//!
//! Invariants:
//! * a connection is in at most one room (one active call per user);
//! * every outbound queue is bounded; a peer that cannot keep up is disconnected;
//! * the server never inspects or logs SDP/ICE contents or media datagrams;
//! * media datagrams for a member are dropped, not queued without bound, when its
//!   connection cannot keep up (like packet loss); they never get it disconnected;
//! * a peer's own direct response to `JoinRoom` is enqueued before any relayed
//!   signal from other members can reach it (both happen under the hub lock).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{Notify, OwnedSemaphorePermit, mpsc};
use turbo_pancake_media_sfu::{Link, PeerSink};
use turbo_pancake_protocol::{self as proto, DecodeError, v1};
use v1::client_envelope::Body as C;
use v1::server_envelope::Body as S;
use v1::{ErrorCode, LeaveReason};

use crate::{AppState, Config, SERVER_VERSION};

pub type PeerId = String;

enum Out {
    Frame { request_id: u64, body: S },
    Close,
}

#[derive(Clone)]
struct PeerHandle {
    display_name: String,
    tx: mpsc::Sender<Out>,
    kill: Arc<Notify>,
}

impl PeerHandle {
    fn send(&self, request_id: u64, body: S) {
        if let Err(mpsc::error::TrySendError::Full(_)) =
            self.tx.try_send(Out::Frame { request_id, body })
        {
            tracing::warn!("outbound queue full, disconnecting slow peer");
            self.kill.notify_one();
        }
        // `Closed` means the peer task is already shutting down and will clean up itself.
    }

    /// Media datagrams: a full queue drops (the receiver conceals), never disconnects.
    fn send_media(&self, body: S) {
        let _ = self.tx.try_send(Out::Frame {
            request_id: 0,
            body,
        });
    }
}

/// Delivers a member's SFU output through its WebSocket.
struct WsSink {
    handle: PeerHandle,
    request_id: u64,
    room: String,
    transport: i32,
}

impl PeerSink for WsSink {
    fn answer(&mut self, result: Result<String, String>) {
        let body = match result {
            Ok(sdp) => S::MediaAnswer(v1::MediaAnswer {
                room_id: self.room.clone(),
                sdp,
                transport: self.transport,
            }),
            Err(e) => {
                tracing::debug!(error = %e, "media offer rejected");
                error_body(ErrorCode::BadRequest, &format!("media offer rejected: {e}"))
            }
        };
        self.handle.send(self.request_id, body);
    }

    fn datagram(&mut self, data: Vec<u8>) {
        self.handle
            .send_media(S::MediaDatagram(v1::MediaDatagram { data }));
    }

    fn track(&mut self, peer: &str, ssrc: u32, published: bool) {
        let (room_id, peer_id) = (self.room.clone(), peer.to_owned());
        let body = if published {
            S::TrackPublished(v1::TrackPublished {
                room_id,
                peer_id,
                ssrc,
            })
        } else {
            S::TrackUnpublished(v1::TrackUnpublished {
                room_id,
                peer_id,
                ssrc,
            })
        };
        // Control events: a member that cannot take them is too slow and gets dropped.
        self.handle.send(0, body);
    }
}

fn error_body(code: ErrorCode, message: &str) -> S {
    S::Error(v1::Error {
        code: code as i32,
        message: message.to_owned(),
    })
}

#[derive(Default)]
pub struct Hub {
    rooms: Mutex<HashMap<String, HashMap<PeerId, PeerHandle>>>,
    next_peer: AtomicU64,
}

impl Hub {
    fn new_peer_id(&self) -> PeerId {
        format!("p{}", self.next_peer.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// Number of peers currently in `room` (0 if the room does not exist).
    pub fn room_size(&self, room: &str) -> usize {
        self.rooms
            .lock()
            .expect("hub lock poisoned")
            .get(room)
            .map_or(0, HashMap::len)
    }

    pub fn room_count(&self) -> usize {
        self.rooms.lock().expect("hub lock poisoned").len()
    }

    fn join(
        &self,
        room: &str,
        peer_id: &str,
        me: PeerHandle,
        request_id: u64,
        max_peers: usize,
    ) -> Result<(), ErrorCode> {
        let mut rooms = self.rooms.lock().expect("hub lock poisoned");
        let members = rooms.entry(room.to_owned()).or_default();
        if members.len() >= max_peers {
            return Err(ErrorCode::RoomFull);
        }
        let peers = members
            .iter()
            .map(|(id, h)| v1::Peer {
                peer_id: id.clone(),
                display_name: h.display_name.clone(),
            })
            .collect();
        me.send(
            request_id,
            S::RoomJoined(v1::RoomJoined {
                room_id: room.to_owned(),
                peers,
            }),
        );
        let announce = v1::PeerJoined {
            room_id: room.to_owned(),
            peer: Some(v1::Peer {
                peer_id: peer_id.to_owned(),
                display_name: me.display_name.clone(),
            }),
        };
        for h in members.values() {
            h.send(0, S::PeerJoined(announce.clone()));
        }
        members.insert(peer_id.to_owned(), me);
        Ok(())
    }

    fn leave(&self, room: &str, peer_id: &str, reason: LeaveReason) -> bool {
        let mut rooms = self.rooms.lock().expect("hub lock poisoned");
        let Some(members) = rooms.get_mut(room) else {
            return false;
        };
        if members.remove(peer_id).is_none() {
            return false;
        }
        let event = v1::PeerLeft {
            room_id: room.to_owned(),
            peer_id: peer_id.to_owned(),
            reason: reason as i32,
        };
        for h in members.values() {
            h.send(0, S::PeerLeft(event.clone()));
        }
        if members.is_empty() {
            rooms.remove(room);
        }
        true
    }

    fn relay(&self, from: &str, signal: v1::Signal) -> Result<(), ErrorCode> {
        let rooms = self.rooms.lock().expect("hub lock poisoned");
        let members = rooms.get(&signal.room_id).ok_or(ErrorCode::NotInRoom)?;
        if !members.contains_key(from) {
            return Err(ErrorCode::NotInRoom);
        }
        let target = members
            .get(&signal.to_peer_id)
            .ok_or(ErrorCode::PeerNotFound)?;
        target.send(
            0,
            S::Signal(v1::SignalRelay {
                room_id: signal.room_id,
                from_peer_id: from.to_owned(),
                payload: signal.payload.map(proto::relay_payload),
            }),
        );
        Ok(())
    }
}

pub async fn ws_handler(State(state): State<AppState>, ws: WebSocketUpgrade) -> Response {
    let Ok(permit) = state.connections.clone().try_acquire_owned() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "too many connections").into_response();
    };
    // Small buffers: signaling frames are tiny and most connections idle; the
    // 128 KiB tungstenite default cost ~140 KB per idle connection (see
    // benchmarks/results). Larger frames still work, the buffer just grows.
    ws.max_message_size(proto::MAX_FRAME_BYTES)
        .max_frame_size(proto::MAX_FRAME_BYTES)
        .read_buffer_size(8 * 1024)
        .write_buffer_size(8 * 1024)
        .on_upgrade(move |socket| connection(socket, state, permit))
}

struct Session {
    peer_id: Option<PeerId>,
    display_name: String,
    room: Option<String>,
    /// This connection has a media session in the SFU.
    media: bool,
    me_tx: mpsc::Sender<Out>,
    kill: Arc<Notify>,
}

impl Session {
    fn handle(&self) -> PeerHandle {
        PeerHandle {
            display_name: self.display_name.clone(),
            tx: self.me_tx.clone(),
            kill: self.kill.clone(),
        }
    }

    fn reply(&self, request_id: u64, body: S) {
        self.handle().send(request_id, body);
    }

    fn reply_error(&self, request_id: u64, code: ErrorCode, message: &str) {
        self.reply(request_id, error_body(code, message));
    }
}

enum Flow {
    Continue,
    Close,
}

async fn connection(socket: WebSocket, state: AppState, _permit: OwnedSemaphorePermit) {
    let cfg = state.config.clone();
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::channel::<Out>(cfg.outbound_queue);

    let mut writer = tokio::spawn(async move {
        let mut event_id = 0u64;
        while let Some(out) = rx.recv().await {
            match out {
                Out::Frame { request_id, body } => {
                    event_id += 1;
                    let env = v1::ServerEnvelope {
                        protocol_version: proto::PROTOCOL_VERSION,
                        request_id,
                        event_id,
                        body: Some(body),
                    };
                    if sink
                        .send(Message::Binary(proto::encode(&env).into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Out::Close => break,
            }
        }
        let _ = sink.send(Message::Close(None)).await;
        let _ = sink.close().await;
    });

    let mut session = Session {
        peer_id: None,
        display_name: String::new(),
        room: None,
        media: false,
        me_tx: tx,
        kill: Arc::new(Notify::new()),
    };

    let reason = loop {
        let kill = session.kill.clone();
        let next = tokio::select! {
            _ = kill.notified() => break LeaveReason::Disconnected,
            r = tokio::time::timeout(cfg.heartbeat_timeout, stream.next()) => r,
        };
        let msg = match next {
            Err(_) => break LeaveReason::Timeout,
            Ok(None) | Ok(Some(Err(_))) => break LeaveReason::Disconnected,
            Ok(Some(Ok(m))) => m,
        };
        match msg {
            Message::Binary(bytes) => {
                if let Flow::Close = handle_frame(&bytes, &mut session, &state) {
                    break LeaveReason::Disconnected;
                }
            }
            Message::Text(_) => session.reply_error(0, ErrorCode::BadRequest, "binary frames only"),
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Close(_) => break LeaveReason::Disconnected,
        }
    };

    if let (Some(peer_id), Some(room)) = (&session.peer_id, session.room.take()) {
        state.hub.leave(&room, peer_id, reason);
    }
    if let (Some(peer_id), true, Some(sfu)) = (&session.peer_id, session.media, &state.sfu) {
        sfu.leave(peer_id);
    }
    tracing::debug!(peer = ?session.peer_id, ?reason, "connection closed");
    let _ = session.me_tx.try_send(Out::Close);
    drop(session);
    if tokio::time::timeout(std::time::Duration::from_secs(2), &mut writer)
        .await
        .is_err()
    {
        writer.abort();
    }
}

/// UDP candidate addresses for a member: the address it dialed first (behind a port
/// forward that is the public one), then configured public addresses, then the LAN one.
fn candidate_ips(dialed: &str, state: &AppState) -> Vec<IpAddr> {
    dialed
        .parse::<IpAddr>()
        .ok()
        .filter(|ip| !ip.is_unspecified())
        .into_iter()
        .chain(state.config.public_ips.iter().copied())
        .chain(state.lan_ip)
        .collect()
}

fn handle_frame(bytes: &[u8], s: &mut Session, state: &AppState) -> Flow {
    let hub: &Hub = &state.hub;
    let cfg: &Config = &state.config;
    let env = match proto::decode_client(bytes) {
        Ok(env) => env,
        Err(DecodeError::UnsupportedVersion(_)) => {
            s.reply_error(
                0,
                ErrorCode::UnsupportedVersion,
                "unsupported protocol version",
            );
            return Flow::Close;
        }
        Err(DecodeError::TooLarge(_)) => {
            s.reply_error(0, ErrorCode::PayloadTooLarge, "frame too large");
            return Flow::Close;
        }
        Err(e) => {
            s.reply_error(0, ErrorCode::BadRequest, &e.to_string());
            return Flow::Continue;
        }
    };
    let rid = env.request_id;
    let Some(body) = env.body else {
        unreachable!("decode_client rejects empty bodies")
    };

    let Some(peer_id) = s.peer_id.clone() else {
        let C::Hello(hello) = body else {
            s.reply_error(rid, ErrorCode::HelloRequired, "send Hello first");
            return Flow::Continue;
        };
        if let Err(e) = proto::validate_hello(&hello) {
            s.reply_error(rid, ErrorCode::BadRequest, &e.to_string());
            return Flow::Continue;
        }
        let id = hub.new_peer_id();
        s.display_name = hello.display_name;
        s.peer_id = Some(id.clone());
        tracing::debug!(peer = %id, client = %hello.client_version, "hello");
        s.reply(
            rid,
            S::Welcome(v1::Welcome {
                peer_id: id,
                server_version: SERVER_VERSION.to_owned(),
                heartbeat_interval_ms: cfg.heartbeat_interval.as_millis() as u32,
                max_frame_bytes: proto::MAX_FRAME_BYTES as u32,
            }),
        );
        return Flow::Continue;
    };

    match body {
        C::Hello(_) => s.reply_error(rid, ErrorCode::BadRequest, "duplicate Hello"),
        C::Ping(p) => s.reply(
            rid,
            S::Pong(v1::Pong {
                client_time_us: p.client_time_us,
            }),
        ),
        C::JoinRoom(j) => {
            if let Err(e) = proto::validate_room_id(&j.room_id) {
                s.reply_error(rid, ErrorCode::BadRequest, &e.to_string());
            } else if s.room.is_some() {
                s.reply_error(
                    rid,
                    ErrorCode::AlreadyInRoom,
                    "leave the current room first",
                );
            } else {
                match hub.join(&j.room_id, &peer_id, s.handle(), rid, cfg.max_room_peers) {
                    Ok(()) => s.room = Some(j.room_id),
                    Err(code) => s.reply_error(rid, code, "cannot join room"),
                }
            }
        }
        C::LeaveRoom(l) => {
            if s.room.as_deref() != Some(l.room_id.as_str()) {
                s.reply_error(rid, ErrorCode::NotInRoom, "not a member of this room");
            } else {
                hub.leave(&l.room_id, &peer_id, LeaveReason::Left);
                s.room = None;
                if let (true, Some(sfu)) = (s.media, &state.sfu) {
                    sfu.leave(&peer_id);
                    s.media = false;
                }
                s.reply(rid, S::RoomLeft(v1::RoomLeft { room_id: l.room_id }));
            }
        }
        C::Signal(sig) => {
            if let Err(e) = proto::validate_signal(&sig) {
                s.reply_error(rid, ErrorCode::BadRequest, &e.to_string());
            } else if sig.to_peer_id == peer_id {
                s.reply_error(rid, ErrorCode::BadRequest, "cannot signal self");
            } else if let Err(code) = hub.relay(&peer_id, sig) {
                s.reply_error(rid, code, "signal not delivered");
            }
        }
        C::MediaOffer(m) => {
            if let Err(e) = proto::validate_media_offer(&m) {
                s.reply_error(rid, ErrorCode::BadRequest, &e.to_string());
            } else if s.room.as_deref() != Some(m.room_id.as_str()) {
                s.reply_error(rid, ErrorCode::NotInRoom, "join the room first");
            } else if let Some(sfu) = &state.sfu {
                let transport = m.transport;
                let link = match m.transport() {
                    v1::MediaTransport::Websocket => Link::Tunnel,
                    _ => Link::Udp {
                        candidate_ips: candidate_ips(&m.dialed_ip, state),
                    },
                };
                let sink = WsSink {
                    handle: s.handle(),
                    request_id: rid,
                    room: m.room_id.clone(),
                    transport,
                };
                if sfu.join(&peer_id, &m.room_id, m.sdp, link, Box::new(sink)) {
                    s.media = true;
                } else {
                    s.reply_error(rid, ErrorCode::Internal, "media server busy");
                }
            } else {
                s.reply_error(rid, ErrorCode::Internal, "media unavailable");
            }
        }
        C::MediaDatagram(d) => {
            // Invalid or unexpected datagrams are dropped silently, like bad UDP.
            if let (true, Some(sfu), Ok(())) = (s.media, &state.sfu, proto::validate_datagram(&d)) {
                sfu.datagram(&peer_id, d.data);
            }
        }
    }
    Flow::Continue
}
