//! Client session (S1/S2): signaling connection, audio engine and the room's media session
//! with the server's SFU.
//!
//! ```text
//! UI ─▶ bounded Command channel ─▶ session thread (Tokio, current-thread runtime)
//!          │  WebSocket /v1/signal: hello, join/leave room, media offer/answer, ping,
//!          │                        tunneled media datagrams
//!          │  AudioEngine (own thread) ── pump thread ─▶ CallDriver (own thread)
//!          │        CallDriver ─▶ UDP to the server, or datagrams through the session
//!          │  CallDriver events ── bridge thread ─▶ session thread
//!          └─▶ on_update(&Snapshot) every `status_interval` and after every change
//! ```
//!
//! Network work, the audio engine and the UI never wait on each other (spec §3): the
//! session thread only does signaling and bookkeeping; media runs on the engine and call
//! threads, connected by bounded non-blocking channels.
//!
//! Media goes through the server, never directly between members: the client offers one
//! WebRTC session (ICE + DTLS-SRTP, Opus) to the SFU after joining a room, and the server
//! forwards every other member's audio on it, one RTP stream (SSRC) per member, announced
//! by `TrackPublished` (ADR 0008). Each stream is its own engine peer (id = SSRC) with
//! its own jitter buffer and volume. The client always sends first, so this works
//! behind NATs and VPNs. With [`TransportPref::Auto`] it tries UDP to the server first and
//! falls back to tunneling the same encrypted datagrams through the signaling WebSocket
//! when UDP does not connect within `udp_attempt_timeout` (VPN without UDP, port not
//! forwarded); after a failure it alternates. Room model for S1 (accounts arrive in S5):
//! a room is joined by id. Leaving the room stops media and the audio engine, so every
//! device, thread and socket is released. A lost signaling connection ends the media
//! session and is retried with backoff; the room is rejoined.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::tungstenite::Message;
use turbo_pancake_media_audio::{
    AudioEngine, AudioError, AudioIo, DspConfig, EngineConfig, EngineStatus, IncomingPacket,
    PacketSink, PeerId, PeerStatus, VoiceLink,
};
use turbo_pancake_media_transport::call::{
    AudioOut, CallConfig, CallDriver, CallEvent, CallState, CallStats, LocalSignal, MediaLink, Role,
};
use turbo_pancake_protocol::{self as proto, v1};
use v1::{client_envelope::Body as C, server_envelope::Body as S};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// Which way media travels to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportPref {
    /// UDP first, the WebSocket tunnel when UDP does not connect.
    Auto,
    Udp,
    /// Always through the signaling WebSocket (TCP).
    Tunnel,
}

/// Transport of the current media session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaTransport {
    Udp,
    Tunnel,
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Signaling endpoint, e.g. `ws://127.0.0.1:7878/v1/signal`.
    pub server_url: String,
    pub display_name: String,
    pub client_version: String,
    /// Devices, mute and DSP; kept up to date by commands and used on every engine start.
    pub engine: EngineConfig,
    /// Local UDP address for media (port 0 = ephemeral, 0.0.0.0 = all interfaces).
    pub media_bind: SocketAddr,
    /// Host candidate IP when `media_bind` is unspecified (default: default route).
    pub advertise: Option<IpAddr>,
    pub transport: TransportPref,
    /// How long a UDP attempt may take before `Auto` switches to the tunnel.
    pub udp_attempt_timeout: Duration,
    /// Test impairment: share of outgoing media datagrams dropped.
    pub send_loss: f32,
    /// Test impairment: every UDP datagram is dropped (a VPN that carries no UDP).
    pub block_udp: bool,
    /// STUN servers (`host:port`) asked for a server-reflexive candidate. Not needed with
    /// the SFU (the server answers to whatever address our datagrams come from).
    pub stun_servers: Vec<String>,
    pub status_interval: Duration,
}

impl SessionConfig {
    pub fn new(server_url: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            server_url: server_url.into(),
            display_name: display_name.into(),
            client_version: format!(
                "turbo-pancake-desktop/{} ({})",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::OS
            ),
            engine: EngineConfig::default(),
            media_bind: SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
            advertise: None,
            transport: TransportPref::Auto,
            udp_attempt_timeout: Duration::from_secs(3),
            send_loss: 0.0,
            block_udp: false,
            stun_servers: Vec::new(),
            status_interval: Duration::from_millis(250),
        }
    }
}

/// UI → session.
#[derive(Debug, Clone)]
pub enum Command {
    Join(String),
    Leave,
    SetMuted(bool),
    SetDsp(DspConfig),
    /// Device id from `device::list_devices` (`None` = system default).
    SetInput(Option<String>),
    SetOutput(Option<String>),
    /// Local playback gain of the whole call (every member), 0.0–4.0.
    SetRemoteVolume(f32),
    /// Local playback gain of one member (by peer id), 0.0–4.0, on top of the call volume.
    SetMemberVolume(String, f32),
    /// Voice activation threshold in dBFS; `None` = always send.
    SetVoiceGate(Option<f32>),
    Quit,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum ServerStatus {
    #[default]
    Connecting,
    Connected,
    /// Reason of the last failure; a reconnect is pending.
    Disconnected(String),
}

/// The media session with the server.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaStatus {
    pub transport: MediaTransport,
    pub state: CallState,
}

/// Another member of the room.
#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub peer_id: String,
    pub name: String,
    /// Their stream on our media session (ADR 0008), once the server announced it.
    pub ssrc: Option<u32>,
    /// Local playback gain set for them (before the call volume), 0.0–4.0.
    pub volume: f32,
    /// Their audio in the engine (jitter buffer, decoder, speaking) while it plays.
    pub stream: Option<PeerStatus>,
}

impl Member {
    /// Speaking indicator: their audio is above the speech level right now.
    pub fn speaking(&self) -> bool {
        self.stream.as_ref().is_some_and(|p| p.stats.speaking)
    }
}

/// Everything the UI shows, published by the session thread.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub server: ServerStatus,
    /// Our connection id on the server while connected.
    pub peer_id: Option<String>,
    /// Room the user asked to be in.
    pub room: Option<String>,
    /// The server confirmed membership of `room`.
    pub joined: bool,
    /// The other members, in join order.
    pub members: Vec<Member>,
    pub media: Option<MediaStatus>,
    pub engine: Option<EngineStatus>,
    pub call: Option<CallStats>,
    pub signaling_rtt_ms: Option<f64>,
    /// Media sessions started so far (transport switches and restarts included).
    pub media_sessions: u64,
    pub last_error: Option<String>,
}

impl Snapshot {
    pub fn member_names(&self) -> Vec<String> {
        self.members.iter().map(|m| m.name.clone()).collect()
    }

    /// Media is up and somebody else is in the room.
    pub fn in_call(&self) -> bool {
        !self.members.is_empty()
            && self
                .media
                .as_ref()
                .is_some_and(|m| m.state == CallState::Connected)
    }
}

type EngineStarter =
    Box<dyn FnMut(EngineConfig) -> Result<(AudioEngine, VoiceLink), AudioError> + Send>;

/// Handle to the session thread. Dropping it quits (leaves the room, releases devices).
pub struct Session {
    commands: mpsc::Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    /// Starts the session thread. `io` opens the audio devices each time the engine
    /// starts (on joining a room); `on_update` runs on the session thread and must not
    /// block for long.
    pub fn start<I, F, U>(config: SessionConfig, mut io: F, on_update: U) -> std::io::Result<Self>
    where
        I: AudioIo,
        F: FnMut() -> I + Send + 'static,
        U: FnMut(&Snapshot) + Send + 'static,
    {
        turbo_pancake_media_transport::init_crypto();
        let (tx, rx) = mpsc::channel(64);
        let start_engine: EngineStarter = Box::new(move |cfg| AudioEngine::start(io(), cfg));
        let thread = std::thread::Builder::new()
            .name("tp-session".into())
            .spawn(move || {
                let mut core = Core::new(config, start_engine, Box::new(on_update));
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt.block_on(core.run(rx)),
                    Err(e) => {
                        core.snap.server = ServerStatus::Disconnected(format!("runtime: {e}"));
                        core.publish();
                    }
                }
            })?;
        Ok(Self {
            commands: tx,
            thread: Some(thread),
        })
    }

    /// Queues a command; `false` if the queue is full or the session ended.
    pub fn send(&self, cmd: Command) -> bool {
        self.commands.try_send(cmd).is_ok()
    }

    /// A cloneable sender for UI callbacks.
    pub fn sender(&self) -> CommandSender {
        CommandSender(self.commands.clone())
    }

    /// Leaves the room, closes the connection, releases audio and waits for the thread.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(t) = self.thread.take() {
            // A full queue still gets the quit: the session drains it continuously.
            let _ = self.commands.blocking_send(Command::Quit);
            let _ = t.join();
        }
    }
}

#[derive(Clone)]
pub struct CommandSender(mpsc::Sender<Command>);

impl CommandSender {
    /// Never blocks; `false` if the queue is full or the session ended.
    pub fn send(&self, cmd: Command) -> bool {
        self.0.try_send(cmd).is_ok()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Audio {
    engine: AudioEngine,
    sink: PacketSink,
    /// SSRCs whose packets reach the engine (checked on the call thread): a stream that
    /// was dropped must not come back as a peer from packets still in flight.
    accepted: Arc<Mutex<HashSet<u32>>>,
    /// Where the pump forwards encoded mic frames: the current media session, if any.
    route: Arc<Mutex<Option<SyncSender<AudioOut>>>>,
    pump: JoinHandle<()>,
}

struct Media {
    driver: CallDriver,
    generation: u64,
    transport: MediaTransport,
    state: CallState,
    /// Request id of our MediaOffer (an Error with it fails this session).
    offer_rid: u64,
    /// Tunnel: datagrams from the server go here.
    tunnel_in: Option<SyncSender<Vec<u8>>>,
    bridge: JoinHandle<()>,
}

struct Core {
    cfg: SessionConfig,
    start_engine: EngineStarter,
    on_update: Box<dyn FnMut(&Snapshot) + Send>,
    snap: Snapshot,
    dirty: bool,
    online: bool,
    /// Server IP the signaling connection reached (offered back as the UDP candidate).
    dialed_ip: Option<IpAddr>,
    want_room: Option<String>,
    joined: Option<String>,
    join_rid: u64,
    /// Other room members: peer id → (join order, display name).
    members: BTreeMap<String, (u64, String)>,
    member_order: u64,
    /// Other members' streams on our media session: peer id → SSRC (= engine peer id).
    tracks: HashMap<String, u32>,
    /// Per-member playback gain set by the user: peer id → gain.
    member_volumes: HashMap<String, f32>,
    audio: Option<Audio>,
    media: Option<Media>,
    /// Transport of the next media session.
    next_transport: MediaTransport,
    generation: u64,
    call_tx: mpsc::UnboundedSender<(u64, CallEvent)>,
    call_rx: Option<mpsc::UnboundedReceiver<(u64, CallEvent)>>,
    tunnel_tx: mpsc::UnboundedSender<(u64, Vec<u8>)>,
    tunnel_rx: Option<mpsc::UnboundedReceiver<(u64, Vec<u8>)>>,
    outbox: Vec<Vec<u8>>,
    next_rid: u64,
    remote_volume: f32,
    epoch: Instant,
    quit: bool,
}

type Ws = tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

impl Core {
    fn new(
        cfg: SessionConfig,
        start_engine: EngineStarter,
        on_update: Box<dyn FnMut(&Snapshot) + Send>,
    ) -> Self {
        let (call_tx, call_rx) = mpsc::unbounded_channel();
        let (tunnel_tx, tunnel_rx) = mpsc::unbounded_channel();
        let next_transport = first_transport(cfg.transport);
        Self {
            cfg,
            start_engine,
            on_update,
            snap: Snapshot::default(),
            dirty: true,
            online: false,
            dialed_ip: None,
            want_room: None,
            joined: None,
            join_rid: 0,
            members: BTreeMap::new(),
            member_order: 0,
            tracks: HashMap::new(),
            member_volumes: HashMap::new(),
            audio: None,
            media: None,
            next_transport,
            generation: 0,
            call_tx,
            call_rx: Some(call_rx),
            tunnel_tx,
            tunnel_rx: Some(tunnel_rx),
            outbox: Vec::new(),
            next_rid: 1,
            remote_volume: 1.0,
            epoch: Instant::now(),
            quit: false,
        }
    }

    async fn run(&mut self, mut cmds: mpsc::Receiver<Command>) {
        let mut call_rx = self.call_rx.take().expect("run once");
        let mut tunnel_rx = self.tunnel_rx.take().expect("run once");
        let mut backoff = Duration::from_millis(500);
        while !self.quit {
            self.snap.server = ServerStatus::Connecting;
            self.publish();
            let reason = match self.connect().await {
                Ok((ws, heartbeat)) => {
                    backoff = Duration::from_millis(500);
                    self.connected(ws, heartbeat, &mut cmds, &mut call_rx, &mut tunnel_rx)
                        .await
                }
                Err(e) => e,
            };
            self.disconnected();
            if self.quit {
                break;
            }
            self.snap.server = ServerStatus::Disconnected(reason);
            self.publish();
            // Back off, still serving the UI (join/leave/mute work while offline).
            let until = tokio::time::Instant::now() + backoff;
            backoff = (backoff * 2).min(MAX_BACKOFF);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(until) => break,
                    c = cmds.recv() => self.command(c.unwrap_or(Command::Quit)),
                    // Leftovers of media sessions that already ended.
                    Some(_) = call_rx.recv() => {}
                    Some(_) = tunnel_rx.recv() => {}
                }
                self.outbox.clear();
                if self.quit {
                    break;
                }
                if std::mem::take(&mut self.dirty) {
                    self.publish();
                }
            }
        }
        self.stop_audio();
        self.snap.server = ServerStatus::Disconnected("вышли".into());
        self.publish();
    }

    /// WebSocket + Hello/Welcome. Returns the heartbeat interval.
    async fn connect(&mut self) -> Result<(Ws, Duration), String> {
        let (mut ws, _) = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async(self.cfg.server_url.as_str()),
        )
        .await
        .map_err(|_| "таймаут подключения".to_string())?
        .map_err(|e| e.to_string())?;
        self.dialed_ip = match ws.get_ref() {
            MaybeTlsStream::Plain(tcp) => tcp.peer_addr().ok().map(|a| a.ip()),
            _ => None,
        };
        let hello = C::Hello(v1::Hello {
            client_version: self.cfg.client_version.clone(),
            display_name: self.cfg.display_name.clone(),
        });
        let rid = self.rid();
        ws.send(Message::Binary(
            proto::encode(&proto::client(rid, hello)).into(),
        ))
        .await
        .map_err(|e| e.to_string())?;
        let welcome = tokio::time::timeout(CONNECT_TIMEOUT, async {
            while let Some(msg) = ws.next().await {
                let Message::Binary(b) = msg.map_err(|e| e.to_string())? else {
                    continue;
                };
                let env = proto::decode_server(&b).map_err(|e| e.to_string())?;
                match env.body {
                    Some(S::Welcome(w)) => return Ok(w),
                    Some(S::Error(e)) => return Err(format!("сервер: {}", e.message)),
                    _ => {}
                }
            }
            Err("соединение закрыто".to_string())
        })
        .await
        .map_err(|_| "нет ответа на Hello".to_string())??;
        self.snap.peer_id = Some(welcome.peer_id);
        let heartbeat = Duration::from_millis(u64::from(welcome.heartbeat_interval_ms.max(1000)));
        Ok((ws, heartbeat))
    }

    async fn connected(
        &mut self,
        ws: Ws,
        heartbeat: Duration,
        cmds: &mut mpsc::Receiver<Command>,
        call_rx: &mut mpsc::UnboundedReceiver<(u64, CallEvent)>,
        tunnel_rx: &mut mpsc::UnboundedReceiver<(u64, Vec<u8>)>,
    ) -> String {
        self.online = true;
        self.snap.server = ServerStatus::Connected;
        self.dirty = true;
        // A new connection may take another network path: start with UDP again.
        self.next_transport = first_transport(self.cfg.transport);
        if let Some(room) = self.want_room.clone() {
            self.send_join(room);
        }
        let (mut sink, mut stream) = ws.split();
        let mut ping = tokio::time::interval(heartbeat);
        let mut status = tokio::time::interval(self.cfg.status_interval);
        let mut last_rx = Instant::now();
        loop {
            for frame in self.outbox.drain(..) {
                if let Err(e) = sink.send(Message::Binary(frame.into())).await {
                    return e.to_string();
                }
            }
            if self.quit {
                let _ = sink.send(Message::Close(None)).await;
                return "вышли".into();
            }
            if std::mem::take(&mut self.dirty) {
                self.publish();
            }
            tokio::select! {
                msg = stream.next() => match msg {
                    Some(Ok(Message::Binary(b))) => {
                        last_rx = Instant::now();
                        match proto::decode_server(&b) {
                            Ok(env) => self.server_event(env),
                            Err(e) => return format!("плохой кадр от сервера: {e}"),
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => return "сервер закрыл соединение".into(),
                    Some(Ok(_)) => last_rx = Instant::now(),
                    Some(Err(e)) => return e.to_string(),
                },
                c = cmds.recv() => self.command(c.unwrap_or(Command::Quit)),
                Some((generation, ev)) = call_rx.recv() => self.call_event(generation, ev),
                Some((generation, data)) = tunnel_rx.recv() => {
                    // Only the current tunnel session's datagrams reach the server.
                    if self.media.as_ref().is_some_and(|m| m.generation == generation) {
                        self.push(C::MediaDatagram(v1::MediaDatagram { data }));
                    }
                }
                _ = ping.tick() => {
                    if last_rx.elapsed() > heartbeat * 3 {
                        return "сервер не отвечает".into();
                    }
                    let now = self.epoch.elapsed().as_micros() as u64;
                    self.push(C::Ping(v1::Ping { client_time_us: now }));
                }
                _ = status.tick() => self.publish(),
            }
        }
    }

    /// The signaling connection is gone: the server dropped us from the room and the SFU,
    /// so media ends. The engine keeps running while the user still wants the room.
    fn disconnected(&mut self) {
        self.online = false;
        self.end_media();
        self.joined = None;
        self.members.clear();
        self.tracks.clear();
        // Peer ids are per connection (accounts arrive in S5): the volumes cannot follow.
        self.member_volumes.clear();
        self.outbox.clear();
        self.snap.peer_id = None;
        self.snap.signaling_rtt_ms = None;
        self.dirty = true;
    }

    fn rid(&mut self) -> u64 {
        let rid = self.next_rid;
        self.next_rid += 1;
        rid
    }

    fn push(&mut self, body: C) -> u64 {
        let rid = self.rid();
        self.outbox.push(proto::encode(&proto::client(rid, body)));
        rid
    }

    fn send_join(&mut self, room: String) {
        if self.online {
            self.join_rid = self.push(C::JoinRoom(v1::JoinRoom { room_id: room }));
        }
    }

    fn command(&mut self, cmd: Command) {
        self.dirty = true;
        match cmd {
            Command::Join(room) => {
                if let Err(e) = proto::validate_room_id(&room) {
                    self.snap.last_error = Some(e.to_string());
                    return;
                }
                if self.want_room.as_deref() == Some(room.as_str()) {
                    return;
                }
                self.leave_room();
                self.snap.last_error = None;
                self.want_room = Some(room.clone());
                if self.audio.is_none() {
                    self.start_audio();
                }
                self.send_join(room);
            }
            Command::Leave => {
                self.leave_room();
                self.want_room = None;
                self.stop_audio();
            }
            Command::SetMuted(m) => {
                self.cfg.engine.muted = m;
                self.with_engine(|e| e.set_muted(m));
            }
            Command::SetDsp(d) => {
                self.cfg.engine.dsp = d;
                self.with_engine(|e| e.set_dsp(d));
            }
            Command::SetInput(id) => {
                self.cfg.engine.input = id.clone();
                self.with_engine(|e| e.set_input_device(id));
            }
            Command::SetOutput(id) => {
                self.cfg.engine.output = id.clone();
                self.with_engine(|e| e.set_output_device(id));
            }
            Command::SetRemoteVolume(v) => {
                self.remote_volume = v.clamp(0.0, 4.0);
                let peers: Vec<(PeerId, f32)> = self
                    .tracks
                    .iter()
                    .map(|(peer, ssrc)| (PeerId::from(*ssrc), self.gain(peer)))
                    .collect();
                self.with_engine(|e| {
                    peers
                        .iter()
                        .try_for_each(|(p, v)| e.set_peer_volume(*p, *v))
                });
            }
            Command::SetMemberVolume(peer, v) => {
                self.member_volumes.insert(peer.clone(), v.clamp(0.0, 4.0));
                if let Some(ssrc) = self.tracks.get(&peer).copied() {
                    let v = self.gain(&peer);
                    self.with_engine(|e| e.set_peer_volume(PeerId::from(ssrc), v));
                }
            }
            Command::SetVoiceGate(g) => {
                self.cfg.engine.voice_gate = g;
                self.with_engine(|e| e.set_voice_gate(g));
            }
            Command::Quit => {
                self.leave_room();
                self.want_room = None;
                self.quit = true;
            }
        }
    }

    /// Engine gain of a member's stream: call volume × their own volume.
    fn gain(&self, peer: &str) -> f32 {
        let own = self.member_volumes.get(peer).copied().unwrap_or(1.0);
        (self.remote_volume * own).clamp(0.0, 4.0)
    }

    fn with_engine(&mut self, f: impl FnOnce(&AudioEngine) -> Result<(), AudioError>) {
        if let Some(a) = &self.audio
            && let Err(e) = f(&a.engine)
        {
            self.snap.last_error = Some(e.to_string());
        }
    }

    /// Ends media and tells the server we left (also for a join still in flight).
    fn leave_room(&mut self) {
        self.end_media();
        self.members.clear();
        self.tracks.clear();
        self.member_volumes.clear();
        let room = self.joined.take().or_else(|| self.want_room.clone());
        if let (Some(room), true) = (room, self.online) {
            self.push(C::LeaveRoom(v1::LeaveRoom { room_id: room }));
        }
    }

    fn add_member(&mut self, peer: v1::Peer) {
        self.member_order += 1;
        self.members
            .insert(peer.peer_id, (self.member_order, peer.display_name));
    }

    fn server_event(&mut self, env: v1::ServerEnvelope) {
        let Some(body) = env.body else { return };
        if !matches!(body, S::MediaDatagram(_) | S::Pong(_)) {
            self.dirty = true;
        }
        match body {
            S::RoomJoined(j) => {
                if self.want_room.as_deref() != Some(j.room_id.as_str()) {
                    // Left (or switched) while the join was in flight.
                    self.push(C::LeaveRoom(v1::LeaveRoom { room_id: j.room_id }));
                    return;
                }
                self.joined = Some(j.room_id);
                self.members.clear();
                for p in j.peers {
                    self.add_member(p);
                }
                self.start_media();
            }
            S::PeerJoined(p) => {
                if self.joined.as_deref() != Some(p.room_id.as_str()) {
                    return;
                }
                if let Some(peer) = p.peer {
                    self.add_member(peer);
                }
            }
            S::PeerLeft(l) => {
                // Its stream ends with TrackUnpublished.
                self.members.remove(&l.peer_id);
                self.member_volumes.remove(&l.peer_id);
            }
            S::TrackPublished(t) => {
                if self.joined.as_deref() != Some(t.room_id.as_str()) {
                    return;
                }
                if let Some(old) = self.tracks.insert(t.peer_id.clone(), t.ssrc)
                    && old != t.ssrc
                {
                    self.drop_source(old);
                }
                self.add_source(&t.peer_id, t.ssrc);
            }
            S::TrackUnpublished(t) => {
                if self.tracks.get(&t.peer_id) == Some(&t.ssrc) {
                    self.tracks.remove(&t.peer_id);
                    self.drop_source(t.ssrc);
                }
            }
            S::MediaAnswer(a) => {
                if let Some(m) = &self.media
                    && env.request_id == m.offer_rid
                    && let Err(e) = m.driver.remote_answer(a.sdp)
                {
                    self.snap.last_error = Some(e.to_string());
                }
            }
            S::MediaDatagram(d) => {
                if let Some(tx) = self.media.as_ref().and_then(|m| m.tunnel_in.as_ref()) {
                    // Full: the call thread is behind; drop like a lost packet.
                    let _ = tx.try_send(d.data);
                }
            }
            S::Pong(p) => {
                let now = self.epoch.elapsed().as_micros() as u64;
                self.snap.signaling_rtt_ms =
                    Some(now.saturating_sub(p.client_time_us) as f64 / 1000.0);
            }
            S::Error(e) => {
                let msg = format!(
                    "сервер: {} ({:?})",
                    e.message,
                    v1::ErrorCode::try_from(e.code).unwrap_or(v1::ErrorCode::Unspecified)
                );
                if env.request_id != 0 && env.request_id == self.join_rid {
                    self.snap.last_error = Some(msg);
                    self.want_room = None;
                    self.joined = None;
                    self.stop_audio();
                } else if self
                    .media
                    .as_ref()
                    .is_some_and(|m| env.request_id != 0 && m.offer_rid == env.request_id)
                {
                    // Offer rejected (e.g. no UDP on the server): try the other way.
                    self.media_failed(msg);
                } else {
                    self.snap.last_error = Some(msg);
                }
            }
            S::RoomLeft(r) => {
                // Our own leave or switch is already done locally (`joined` was cleared
                // first). A RoomLeft for the room we are in means the server ended our
                // membership: the call is over, do not rejoin on our own.
                if self.joined.as_deref() == Some(r.room_id.as_str()) {
                    self.leave_room();
                    self.want_room = None;
                    self.stop_audio();
                    self.snap.last_error =
                        Some(format!("звонок в «{}» завершён сервером", r.room_id));
                }
            }
            S::Welcome(_) | S::Signal(_) => {}
        }
    }

    /// Accepts a member's stream on the media session and gives it their volume.
    fn add_source(&mut self, peer: &str, ssrc: u32) {
        if let Some(m) = &self.media {
            let _ = m.driver.expect_source(ssrc);
        }
        if let Some(a) = &self.audio {
            a.accepted.lock().unwrap().insert(ssrc);
            let _ = a
                .engine
                .set_peer_volume(PeerId::from(ssrc), self.gain(peer));
        }
    }

    /// Stops accepting a stream and drops its jitter buffer.
    fn drop_source(&mut self, ssrc: u32) {
        if let Some(m) = &self.media {
            let _ = m.driver.forget_source(ssrc);
        }
        if let Some(a) = &self.audio {
            a.accepted.lock().unwrap().remove(&ssrc);
            let _ = a.engine.remove_peer(PeerId::from(ssrc));
        }
    }

    fn call_event(&mut self, generation: u64, ev: CallEvent) {
        let Some(media) = self.media.as_mut().filter(|m| m.generation == generation) else {
            return; // from a media session that already ended
        };
        self.dirty = true;
        match ev {
            CallEvent::Signal(LocalSignal::Offer(sdp)) => {
                let (Some(room), transport) = (self.joined.clone(), media.transport) else {
                    return;
                };
                let transport = match transport {
                    MediaTransport::Udp => v1::MediaTransport::Udp,
                    MediaTransport::Tunnel => v1::MediaTransport::Websocket,
                };
                let dialed_ip = self.dialed_ip.map(|ip| ip.to_string()).unwrap_or_default();
                let rid = self.push(C::MediaOffer(v1::MediaOffer {
                    room_id: room,
                    sdp,
                    transport: transport as i32,
                    dialed_ip,
                }));
                if let Some(m) = &mut self.media {
                    m.offer_rid = rid;
                }
            }
            // The SFU is ICE-lite with its candidates in the answer: nothing to trickle.
            CallEvent::Signal(_) => {}
            CallEvent::State(state) => {
                media.state = state.clone();
                if let CallState::Failed(reason) = state {
                    self.media_failed(format!("медиа: {reason}"));
                } else if state == CallState::Connected {
                    self.snap.last_error = None;
                }
            }
        }
    }

    /// The media session failed: switch transport (Auto) and start over while in the room.
    fn media_failed(&mut self, reason: String) {
        let Some(transport) = self.media.as_ref().map(|m| m.transport) else {
            return;
        };
        self.snap.last_error = Some(reason);
        self.end_media();
        if self.cfg.transport == TransportPref::Auto {
            self.next_transport = match transport {
                MediaTransport::Udp => MediaTransport::Tunnel,
                MediaTransport::Tunnel => MediaTransport::Udp,
            };
        }
        self.start_media();
    }

    fn start_audio(&mut self) {
        match (self.start_engine)(self.cfg.engine.clone()) {
            Ok((engine, link)) => {
                let route: Arc<Mutex<Option<SyncSender<AudioOut>>>> = Arc::default();
                let pump_route = route.clone();
                let outgoing = link.outgoing;
                // Ends when the engine stops (its sender is dropped).
                let pump = std::thread::Builder::new()
                    .name("tp-audio-pump".into())
                    .spawn(move || {
                        while let Ok(f) = outgoing.recv() {
                            if let Some(tx) = pump_route.lock().unwrap().as_ref() {
                                // A full queue drops: the call thread is behind.
                                let _ = tx.try_send(AudioOut {
                                    seq: f.seq,
                                    timestamp: f.timestamp,
                                    payload: f.payload,
                                });
                            }
                        }
                    });
                match pump {
                    Ok(pump) => {
                        self.audio = Some(Audio {
                            engine,
                            sink: link.incoming,
                            accepted: Arc::default(),
                            route,
                            pump,
                        });
                    }
                    Err(e) => {
                        engine.stop();
                        self.snap.last_error = Some(format!("аудио: {e}"));
                    }
                }
            }
            Err(e) => self.snap.last_error = Some(format!("аудио: {e}")),
        }
    }

    fn stop_audio(&mut self) {
        self.end_media();
        if let Some(a) = self.audio.take() {
            a.engine.stop();
            let _ = a.pump.join();
        }
        self.dirty = true;
    }

    /// Starts the media session with the SFU over `next_transport` (in the joined room).
    fn start_media(&mut self) {
        if self.media.is_some() || self.joined.is_none() || !self.online {
            return;
        }
        let Some(audio) = &self.audio else {
            self.snap.last_error = Some("нет аудиодвижка".into());
            return;
        };
        let transport = self.next_transport;
        let (tx, rx) = sync_channel::<AudioOut>(50);
        let sink = audio.sink.clone();
        let accepted = audio.accepted.clone();
        let mut cc = CallConfig::new(Role::Offerer, self.cfg.media_bind);
        cc.advertise = self.cfg.advertise;
        cc.send_loss = self.cfg.send_loss;
        cc.stun_servers = self.cfg.stun_servers.clone();
        if transport == MediaTransport::Udp {
            if self.cfg.transport == TransportPref::Auto {
                cc.connect_timeout = self.cfg.udp_attempt_timeout;
            }
            if self.cfg.block_udp {
                cc.send_loss = 1.0;
            }
        }
        self.generation += 1;
        let generation = self.generation;
        let (link, tunnel_in) = match transport {
            MediaTransport::Udp => (MediaLink::Udp, None),
            MediaTransport::Tunnel => {
                // A few seconds of audio + RTCP from the server.
                let (in_tx, in_rx) = sync_channel::<Vec<u8>>(256);
                let out = self.tunnel_tx.clone();
                let outbound = Box::new(move |d: Vec<u8>| {
                    let _ = out.send((generation, d));
                });
                (
                    MediaLink::Tunnel {
                        inbound: in_rx,
                        outbound,
                    },
                    Some(in_tx),
                )
            }
        };
        let started = CallDriver::start_with(cc, link, rx, move |p| {
            if !accepted.lock().unwrap().contains(&p.ssrc) {
                return;
            }
            sink.push(IncomingPacket {
                peer: PeerId::from(p.ssrc),
                seq: p.seq,
                timestamp: p.timestamp,
                payload: p.payload,
                arrival: p.arrival,
            });
        });
        let (driver, events) = match started {
            Ok(d) => d,
            Err(e) => {
                self.snap.last_error = Some(format!("медиа: {e}"));
                return;
            }
        };
        let to_session = self.call_tx.clone();
        // Ends when the call thread exits (its event sender is dropped).
        let bridge = std::thread::Builder::new()
            .name("tp-call-events".into())
            .spawn(move || {
                while let Ok(ev) = events.recv() {
                    if to_session.send((generation, ev)).is_err() {
                        break;
                    }
                }
            });
        let bridge = match bridge {
            Ok(b) => b,
            Err(e) => {
                driver.stop();
                self.snap.last_error = Some(format!("медиа: {e}"));
                return;
            }
        };
        *audio.route.lock().unwrap() = Some(tx);
        self.media = Some(Media {
            driver,
            generation,
            transport,
            state: CallState::Negotiating,
            offer_rid: 0,
            tunnel_in,
            bridge,
        });
        self.snap.media_sessions += 1;
        self.dirty = true;
        // Streams the server announced earlier stay valid until it says otherwise.
        let known: Vec<(String, u32)> = self.tracks.iter().map(|(p, s)| (p.clone(), *s)).collect();
        for (peer, ssrc) in known {
            self.add_source(&peer, ssrc);
        }
    }

    fn end_media(&mut self) {
        let Some(media) = self.media.take() else {
            return;
        };
        if let Some(a) = &self.audio {
            *a.route.lock().unwrap() = None;
        }
        media.driver.stop();
        // The call thread has exited, so the bridge's receiver is closed; it never
        // blocks on the (unbounded) session channel.
        let _ = media.bridge.join();
        // Jitter buffers start over with the next session.
        if let Some(a) = &self.audio {
            a.accepted.lock().unwrap().clear();
            for ssrc in self.tracks.values() {
                let _ = a.engine.remove_peer(PeerId::from(*ssrc));
            }
        }
        self.dirty = true;
    }

    fn publish(&mut self) {
        self.snap.room = self.want_room.clone();
        self.snap.joined = self.joined.is_some();
        self.snap.engine = self.audio.as_ref().map(|a| a.engine.status());
        let mut members: Vec<(&u64, &String, &String)> = self
            .members
            .iter()
            .map(|(peer, (order, name))| (order, peer, name))
            .collect();
        members.sort();
        let streams = self.snap.engine.as_ref().map(|e| e.peers.as_slice());
        self.snap.members = members
            .into_iter()
            .map(|(_, peer, name)| {
                let ssrc = self.tracks.get(peer).copied();
                Member {
                    peer_id: peer.clone(),
                    name: name.clone(),
                    ssrc,
                    volume: self.member_volumes.get(peer).copied().unwrap_or(1.0),
                    stream: ssrc
                        .and_then(|s| streams?.iter().find(|p| p.peer == PeerId::from(s)).cloned()),
                }
            })
            .collect();
        self.snap.call = self.media.as_ref().map(|m| m.driver.stats());
        self.snap.media = self.media.as_ref().map(|m| MediaStatus {
            transport: m.transport,
            state: m.state.clone(),
        });
        (self.on_update)(&self.snap);
    }
}

fn first_transport(pref: TransportPref) -> MediaTransport {
    match pref {
        TransportPref::Tunnel => MediaTransport::Tunnel,
        TransportPref::Auto | TransportPref::Udp => MediaTransport::Udp,
    }
}
