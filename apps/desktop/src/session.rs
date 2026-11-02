//! Client session (S1): signaling connection, audio engine and one 1:1 call.
//!
//! ```text
//! UI ─▶ bounded Command channel ─▶ session thread (Tokio, current-thread runtime)
//!          │  WebSocket /v1/signal: hello, join/leave room, SDP/ICE relay, ping
//!          │  AudioEngine (own thread) ── pump thread ─▶ CallDriver (own thread) ─▶ UDP
//!          │  CallDriver events ── bridge thread ─▶ session thread
//!          └─▶ on_update(&Snapshot) every `status_interval` and after every change
//! ```
//!
//! Network work, the audio engine and the UI never wait on each other (spec §3): the
//! session thread only does signaling and bookkeeping; media runs on the engine and call
//! threads, connected by bounded non-blocking channels.
//!
//! Room model for the S1 milestone (accounts arrive in S5): the first member of a room
//! waits and answers, the member who joins later creates the offer. Leaving the room stops
//! the call and the audio engine, so every device, thread and socket is released. A lost
//! signaling connection ends the call and is retried with backoff; the room is rejoined.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use turbo_pancake_media_audio::{
    AudioEngine, AudioError, AudioIo, DspConfig, EngineConfig, EngineStatus, IncomingPacket,
    PacketSink, PeerId, VoiceLink,
};
use turbo_pancake_media_transport::call::{
    AudioOut, CallConfig, CallDriver, CallEvent, CallState, CallStats, LocalSignal, Role,
};
use turbo_pancake_protocol::{self as proto, v1};
use v1::{client_envelope::Body as C, server_envelope::Body as S};

/// Engine peer id of the remote side of the 1:1 call.
const REMOTE: PeerId = 1;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(5);

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
    /// Test impairment: share of outgoing media datagrams dropped.
    pub send_loss: f32,
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
            send_loss: 0.0,
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
    /// Local playback gain of the remote peer, 0.0–4.0.
    SetRemoteVolume(f32),
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

#[derive(Debug, Clone, PartialEq)]
pub struct RemotePeer {
    pub peer_id: String,
    pub name: String,
    pub role: Role,
    pub state: CallState,
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
    /// Other members of the room.
    pub others: usize,
    pub remote: Option<RemotePeer>,
    pub engine: Option<EngineStatus>,
    pub call: Option<CallStats>,
    pub signaling_rtt_ms: Option<f64>,
    /// Call drivers started so far (restarts after failures included).
    pub calls_started: u64,
    pub last_error: Option<String>,
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
    /// Where the pump forwards encoded mic frames: the current call, if any.
    route: Arc<Mutex<Option<SyncSender<AudioOut>>>>,
    pump: JoinHandle<()>,
}

struct Call {
    driver: CallDriver,
    generation: u64,
    remote: RemotePeer,
    bridge: JoinHandle<()>,
}

struct Core {
    cfg: SessionConfig,
    start_engine: EngineStarter,
    on_update: Box<dyn FnMut(&Snapshot) + Send>,
    snap: Snapshot,
    dirty: bool,
    online: bool,
    want_room: Option<String>,
    joined: Option<String>,
    join_rid: u64,
    /// Other room members: peer id → display name.
    members: HashMap<String, String>,
    audio: Option<Audio>,
    call: Option<Call>,
    generation: u64,
    call_tx: mpsc::UnboundedSender<(u64, CallEvent)>,
    call_rx: Option<mpsc::UnboundedReceiver<(u64, CallEvent)>>,
    outbox: Vec<Vec<u8>>,
    next_rid: u64,
    remote_volume: f32,
    epoch: Instant,
    quit: bool,
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

impl Core {
    fn new(
        cfg: SessionConfig,
        start_engine: EngineStarter,
        on_update: Box<dyn FnMut(&Snapshot) + Send>,
    ) -> Self {
        let (call_tx, call_rx) = mpsc::unbounded_channel();
        Self {
            cfg,
            start_engine,
            on_update,
            snap: Snapshot::default(),
            dirty: true,
            online: false,
            want_room: None,
            joined: None,
            join_rid: 0,
            members: HashMap::new(),
            audio: None,
            call: None,
            generation: 0,
            call_tx,
            call_rx: Some(call_rx),
            outbox: Vec::new(),
            next_rid: 1,
            remote_volume: 1.0,
            epoch: Instant::now(),
            quit: false,
        }
    }

    async fn run(&mut self, mut cmds: mpsc::Receiver<Command>) {
        let mut call_rx = self.call_rx.take().expect("run once");
        let mut backoff = Duration::from_millis(500);
        while !self.quit {
            self.snap.server = ServerStatus::Connecting;
            self.publish();
            let reason = match self.connect().await {
                Ok((ws, heartbeat)) => {
                    backoff = Duration::from_millis(500);
                    self.connected(ws, heartbeat, &mut cmds, &mut call_rx).await
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
                    // Events of calls that already ended.
                    Some(_) = call_rx.recv() => {}
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
    ) -> String {
        self.online = true;
        self.snap.server = ServerStatus::Connected;
        self.dirty = true;
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

    /// The signaling connection is gone: the server dropped us from the room, so the
    /// call ends. The engine keeps running while the user still wants the room.
    fn disconnected(&mut self) {
        self.online = false;
        self.end_call();
        self.joined = None;
        self.members.clear();
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
                let v = self.remote_volume;
                self.with_engine(|e| e.set_peer_volume(REMOTE, v));
            }
            Command::Quit => {
                self.leave_room();
                self.want_room = None;
                self.quit = true;
            }
        }
    }

    fn with_engine(&mut self, f: impl FnOnce(&AudioEngine) -> Result<(), AudioError>) {
        if let Some(a) = &self.audio
            && let Err(e) = f(&a.engine)
        {
            self.snap.last_error = Some(e.to_string());
        }
    }

    /// Ends the call and tells the server we left (also for a join still in flight).
    fn leave_room(&mut self) {
        self.end_call();
        self.members.clear();
        let room = self.joined.take().or_else(|| self.want_room.clone());
        if let (Some(room), true) = (room, self.online) {
            self.push(C::LeaveRoom(v1::LeaveRoom { room_id: room }));
        }
    }

    fn server_event(&mut self, env: v1::ServerEnvelope) {
        self.dirty = true;
        let Some(body) = env.body else { return };
        match body {
            S::RoomJoined(j) => {
                if self.want_room.as_deref() != Some(j.room_id.as_str()) {
                    // Left (or switched) while the join was in flight.
                    self.push(C::LeaveRoom(v1::LeaveRoom { room_id: j.room_id }));
                    return;
                }
                self.joined = Some(j.room_id);
                self.members = j
                    .peers
                    .iter()
                    .map(|p| (p.peer_id.clone(), p.display_name.clone()))
                    .collect();
                // We came last: we call the member who was waiting.
                if let Some(p) = j.peers.first() {
                    self.start_call(Role::Offerer, p.peer_id.clone(), p.display_name.clone());
                }
            }
            S::PeerJoined(p) => {
                if self.joined.as_deref() != Some(p.room_id.as_str()) {
                    return;
                }
                let Some(peer) = p.peer else { return };
                self.members
                    .insert(peer.peer_id.clone(), peer.display_name.clone());
                if self.call.is_none() {
                    self.start_call(Role::Answerer, peer.peer_id, peer.display_name);
                }
            }
            S::PeerLeft(l) => {
                self.members.remove(&l.peer_id);
                if self
                    .call
                    .as_ref()
                    .is_some_and(|c| c.remote.peer_id == l.peer_id)
                {
                    self.end_call();
                }
            }
            S::Signal(r) => self.relay_in(r),
            S::Pong(p) => {
                let now = self.epoch.elapsed().as_micros() as u64;
                self.snap.signaling_rtt_ms =
                    Some(now.saturating_sub(p.client_time_us) as f64 / 1000.0);
            }
            S::Error(e) => {
                self.snap.last_error = Some(format!(
                    "сервер: {} ({:?})",
                    e.message,
                    v1::ErrorCode::try_from(e.code).unwrap_or(v1::ErrorCode::Unspecified)
                ));
                if env.request_id != 0 && env.request_id == self.join_rid {
                    self.want_room = None;
                    self.joined = None;
                    self.stop_audio();
                }
            }
            S::Welcome(_) | S::RoomLeft(_) => {}
        }
    }

    fn relay_in(&mut self, r: v1::SignalRelay) {
        use v1::signal_relay::Payload;
        if self.joined.as_deref() != Some(r.room_id.as_str()) {
            return;
        }
        let from = r.from_peer_id;
        let ours = |c: &Option<Call>| c.as_ref().is_some_and(|c| c.remote.peer_id == from);
        match r.payload {
            Some(Payload::Description(d)) => match d.sdp_type() {
                v1::SdpType::Offer => {
                    if self.call.is_some() && !ours(&self.call) {
                        return; // busy with another member (1:1 only in S1)
                    }
                    // A fresh answerer takes the offer; otherwise the remote restarted
                    // the call (after a failure) and we start over as answerer.
                    let fresh = self.call.as_ref().is_some_and(|c| {
                        c.remote.role == Role::Answerer && c.remote.state == CallState::Negotiating
                    });
                    if !fresh {
                        self.end_call();
                        let name = self.members.get(&from).cloned().unwrap_or_default();
                        self.start_call(Role::Answerer, from.clone(), name);
                    }
                    if let Some(c) = &self.call
                        && let Err(e) = c.driver.remote_offer(d.sdp)
                    {
                        self.snap.last_error = Some(e.to_string());
                    }
                }
                v1::SdpType::Answer => {
                    if let Some(c) = &self.call
                        && c.remote.peer_id == from
                        && c.remote.role == Role::Offerer
                        && let Err(e) = c.driver.remote_answer(d.sdp)
                    {
                        self.snap.last_error = Some(e.to_string());
                    }
                }
                v1::SdpType::Unspecified => {}
            },
            Some(Payload::IceCandidate(c)) if !c.candidate.is_empty() => {
                if let Some(call) = &self.call
                    && call.remote.peer_id == from
                    && let Err(e) = call.driver.remote_candidate(c.candidate)
                {
                    self.snap.last_error = Some(e.to_string());
                }
            }
            _ => {}
        }
    }

    fn call_event(&mut self, generation: u64, ev: CallEvent) {
        let Some(call) = self.call.as_mut().filter(|c| c.generation == generation) else {
            return; // from a call that already ended
        };
        self.dirty = true;
        match ev {
            CallEvent::Signal(sig) => {
                use v1::signal::Payload;
                let desc = |t: v1::SdpType, sdp| {
                    Payload::Description(v1::SessionDescription {
                        sdp_type: t as i32,
                        sdp,
                    })
                };
                let payload = match sig {
                    LocalSignal::Offer(sdp) => desc(v1::SdpType::Offer, sdp),
                    LocalSignal::Answer(sdp) => desc(v1::SdpType::Answer, sdp),
                    LocalSignal::Candidate(candidate) => Payload::IceCandidate(v1::IceCandidate {
                        candidate,
                        sdp_mid: String::new(),
                        sdp_mline_index: 0,
                    }),
                };
                let (Some(room), to) = (self.joined.clone(), call.remote.peer_id.clone()) else {
                    return;
                };
                self.push(C::Signal(v1::Signal {
                    room_id: room,
                    to_peer_id: to,
                    payload: Some(payload),
                }));
            }
            CallEvent::State(state) => {
                call.remote.state = state.clone();
                if let CallState::Failed(reason) = state {
                    // Timed out connecting or reconnecting: start over while the peer is
                    // still in the room (the offerer re-offers, the answerer waits).
                    let RemotePeer {
                        peer_id,
                        name,
                        role,
                        ..
                    } = call.remote.clone();
                    self.snap.last_error = Some(format!("звонок: {reason}"));
                    self.end_call();
                    if self.members.contains_key(&peer_id) {
                        self.start_call(role, peer_id, name);
                    }
                }
            }
        }
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
        self.end_call();
        if let Some(a) = self.audio.take() {
            a.engine.stop();
            let _ = a.pump.join();
        }
        self.dirty = true;
    }

    fn start_call(&mut self, role: Role, peer_id: String, name: String) {
        let Some(audio) = &self.audio else {
            self.snap.last_error = Some("звонок без аудиодвижка".into());
            return;
        };
        let (tx, rx) = sync_channel::<AudioOut>(50);
        let sink = audio.sink.clone();
        let mut cc = CallConfig::new(role, self.cfg.media_bind);
        cc.advertise = self.cfg.advertise;
        cc.send_loss = self.cfg.send_loss;
        let started = CallDriver::start(cc, rx, move |p| {
            sink.push(IncomingPacket {
                peer: REMOTE,
                seq: p.seq,
                payload: p.payload,
                arrival: p.arrival,
            });
        });
        let (driver, events) = match started {
            Ok(d) => d,
            Err(e) => {
                self.snap.last_error = Some(format!("звонок: {e}"));
                return;
            }
        };
        self.generation += 1;
        let generation = self.generation;
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
                self.snap.last_error = Some(format!("звонок: {e}"));
                return;
            }
        };
        *audio.route.lock().unwrap() = Some(tx);
        let _ = audio.engine.set_peer_volume(REMOTE, self.remote_volume);
        self.call = Some(Call {
            driver,
            generation,
            remote: RemotePeer {
                peer_id,
                name,
                role,
                state: CallState::Negotiating,
            },
            bridge,
        });
        self.snap.calls_started += 1;
        self.dirty = true;
    }

    fn end_call(&mut self) {
        let Some(call) = self.call.take() else { return };
        if let Some(a) = &self.audio {
            *a.route.lock().unwrap() = None;
        }
        call.driver.stop();
        // The call thread has exited, so the bridge's receiver is closed; it never
        // blocks on the (unbounded) session channel.
        let _ = call.bridge.join();
        if let Some(a) = &self.audio {
            let _ = a.engine.remove_peer(REMOTE);
        }
        self.dirty = true;
    }

    fn publish(&mut self) {
        self.snap.room = self.want_room.clone();
        self.snap.joined = self.joined.is_some();
        self.snap.others = self.members.len();
        self.snap.engine = self.audio.as_ref().map(|a| a.engine.status());
        self.snap.call = self.call.as_ref().map(|c| c.driver.stats());
        self.snap.remote = self.call.as_ref().map(|c| c.remote.clone());
        (self.on_update)(&self.snap);
    }
}
