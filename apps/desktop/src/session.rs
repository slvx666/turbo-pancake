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
//!
//! Screen share (S3, [`crate::screen`]): the media session also carries an H.264 video
//! m-line. Our share (`ScreenShare`: capture → encode thread) feeds it through a bounded
//! route like the microphone; the server is told with `ScreenShare` after every media
//! offer, so a restarted session announces it again. Watching another member's share
//! subscribes to it (`SubscribeScreen`) and routes its packets to the viewer thread; the
//! newest decoded picture waits in a [`PictureSlot`] for the UI. The share's system audio
//! (everything the computer plays except this messenger, `system::SystemAudioCapture`)
//! travels on its own m-line; a viewer plays it as one more engine stream while watching.
//!
//! Layers (S4, ADR 0011): our share also sends a low preview layer (fourth m-line), and
//! its high layer adapts to the uplink (`adapt::Adapter`: bitrate, then size, the frame
//! rate last; pacing follows the bitrate). We show other shares as previews: the visible
//! page of at most `max_previews` shares is subscribed (`SubscribePreviews`) and decoded
//! on one thread ([`PreviewDecoder`]); the rest is not received at all. The watched share
//! asks for a layer (`WatchLayer`); the server says which one flows and why
//! (`ScreenSubscription`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::tungstenite::Message;
use turbo_pancake_media_audio::system::{SystemAudioCapture, SystemAudioStats};
use turbo_pancake_media_audio::{
    AudioEngine, AudioError, AudioIo, DspConfig, EngineConfig, EngineStatus, IncomingPacket,
    PacketSink, PeerId, PeerStatus, VoiceLink,
};
use turbo_pancake_media_transport::call::{
    AudioOut, CallConfig, CallDriver, CallEvent, CallState, CallStats, KeyframeRequester,
    LocalSignal, MediaLink, Role, VideoIo,
};
use turbo_pancake_media_transport::video::{VideoIn, VideoOut};
use turbo_pancake_media_video::adapt::{Adapter, Rung};
use turbo_pancake_media_video::{ScreenShare, ShareConfig, ShareStats, VideoError};

use crate::screen::{
    PictureSlot, PreviewDecoder, PreviewSlots, PreviewStats, Viewer, ViewerStats, rtp_time,
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
    /// Test impairment: share of incoming media datagrams dropped (a lossy downlink).
    pub recv_loss: f32,
    /// Test impairment: every UDP datagram is dropped (a VPN that carries no UDP).
    pub block_udp: bool,
    /// Share the computer's sound together with the screen (where the OS can exclude
    /// our own sound; elsewhere the share runs without it and says so).
    pub share_audio: bool,
    /// STUN servers (`host:port`) asked for a server-reflexive candidate. Not needed with
    /// the SFU (the server answers to whatever address our datagrams come from).
    pub stun_servers: Vec<String>,
    pub status_interval: Duration,
    /// Most previews received and decoded at once (one page of the grid, spec §5.3).
    pub max_previews: usize,
    /// Certificate trust for `wss://` (`None` = the public web PKI). Always verified.
    pub tls: Option<turbo_pancake_messenger_client::api::TlsConfig>,
}

impl SessionConfig {
    pub fn new(server_url: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            server_url: server_url.into(),
            display_name: display_name.into(),
            client_version: format!(
                "turbo-pancake-desktop/{} ({})",
                crate::updater::build_version(),
                std::env::consts::OS
            ),
            engine: EngineConfig::default(),
            media_bind: SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
            advertise: None,
            transport: TransportPref::Auto,
            udp_attempt_timeout: Duration::from_secs(3),
            send_loss: 0.0,
            recv_loss: 0.0,
            block_udp: false,
            share_audio: true,
            stun_servers: Vec::new(),
            status_interval: Duration::from_millis(250),
            max_previews: 24,
            tls: None,
        }
    }
}

/// UI → session.
#[derive(Debug, Clone)]
pub enum Command {
    Join(String),
    JoinConversation {
        token: String,
        participation: v1::CallParticipation,
    },
    SignOut,
    /// Retry the signaling transport while preserving the authorized participation.
    Reconnect,
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
    /// Start sharing (or switch monitor / quality: the running share is replaced).
    StartShare(ShareConfig),
    StopShare,
    /// Send the computer's sound with the share (takes effect at once while sharing).
    SetShareAudio(bool),
    /// Watch this member's share (`None` = none).
    Watch(Option<String>),
    /// The layer to ask for as the watched picture.
    SetWatchLayer(WatchLayer),
    /// Show previews (`false` = receive none), starting at this share of the list.
    SetPreviews {
        visible: bool,
        first: usize,
    },
    Quit,
}

/// Which layer of the watched share to ask for (spec §5.1: per viewer, auto or manual).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WatchLayer {
    #[default]
    Auto,
    High,
    Low,
}

/// What the server says about the watched share's layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerState {
    /// The high layer flows (else the low one).
    pub high: bool,
    pub reason: v1::LayerReason,
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

/// Another member's active share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareView {
    pub peer_id: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// The share has a preview layer.
    pub has_preview: bool,
    /// Its preview stream on our media session while we receive it (key of the
    /// preview pictures).
    pub preview_ssrc: Option<u32>,
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
    /// Other members' active shares, in member order.
    pub shares: Vec<ShareView>,
    /// Whose share we watch.
    pub watching: Option<String>,
    pub viewer: Option<ViewerStats>,
    /// Our own share: settings and sending-side counters.
    pub share_config: Option<ShareConfig>,
    pub sharing: Option<ShareStats>,
    /// Our share's system audio while it runs.
    pub share_audio: Option<SystemAudioStats>,
    /// The watched share's system audio as it plays here.
    pub watched_audio: Option<PeerStatus>,
    pub watch_layer: WatchLayer,
    /// The layer the server forwards for the watched share.
    pub layer: Option<LayerState>,
    pub previews_visible: bool,
    /// First share of the visible preview page, and how many fit on a page.
    pub preview_first: usize,
    pub max_previews: usize,
    pub previews: Option<PreviewStats>,
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
    picture: PictureSlot,
    previews: PreviewSlots,
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
        let picture = PictureSlot::default();
        let core_picture = picture.clone();
        let previews = PreviewSlots::default();
        let core_previews = previews.clone();
        let thread = std::thread::Builder::new()
            .name("tp-session".into())
            .spawn(move || {
                let mut core = Core::new(config, start_engine, Box::new(on_update));
                core.picture = core_picture;
                core.preview_slots = core_previews;
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
            picture,
            previews,
            thread: Some(thread),
        })
    }

    /// The newest preview pictures, by preview SSRC (`ShareView::preview_ssrc`).
    pub fn previews(&self) -> PreviewSlots {
        self.previews.clone()
    }

    /// The newest picture of the watched share (the UI takes it out).
    pub fn picture(&self) -> PictureSlot {
        self.picture.clone()
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

struct Sharing {
    share: ScreenShare,
    config: ShareConfig,
    /// Automatic quality (when `config.adaptive`), created once the source size is known.
    adapter: Option<Adapter>,
    last_adapt: Instant,
    /// Pacer drops seen at the last adaptation step.
    pacing_drops: u64,
    /// NACKs received and video packets sent at the last adaptation step.
    nacks: (u64, u64),
    /// What the server was told (size, fps, bitrate).
    announced: (u32, u32, u32, u32),
}

/// Another member's announced share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShareState {
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    preview: bool,
}

/// Pacing of our main video for a target bitrate: room for keyframes above the average,
/// at least the S3 rate (two repair rounds inside str0m's window on a LAN).
fn pacing_bps(kbps: u32) -> u32 {
    (kbps.saturating_mul(1600)).max(12_000_000)
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
    auth_token: String,
    authorization: Option<v1::CallParticipation>,
    auth_changed: bool,
    pending_leaves: Vec<(String, String, String)>,
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
    share: Option<Sharing>,
    /// Where our share's encoded frames go: the current media session, if any.
    share_route: Arc<Mutex<Option<SyncSender<VideoOut>>>>,
    system_audio: Option<SystemAudioCapture>,
    share_audio_route: Arc<Mutex<Option<SyncSender<AudioOut>>>>,
    /// Other members' share audio streams: peer id → SSRC.
    screen_audio_tracks: HashMap<String, u32>,
    /// The watched share's audio stream, accepted into the engine.
    watched_audio: Option<u32>,
    /// Other members' screen streams on our media session: peer id → (SSRC, RTX SSRC).
    screen_tracks: HashMap<String, (u32, Option<u32>)>,
    /// Other members' active shares.
    shares: HashMap<String, ShareState>,
    /// Other members' preview streams on our media session: peer id → (SSRC, RTX SSRC).
    preview_tracks: HashMap<String, (u32, Option<u32>)>,
    preview_decoder: Option<PreviewDecoder>,
    /// Preview SSRCs routed to the decoder (checked on the call thread).
    preview_ssrcs: Arc<Mutex<HashSet<u32>>>,
    preview_route: Arc<Mutex<Option<SyncSender<VideoIn>>>>,
    preview_slots: PreviewSlots,
    /// The shares whose previews we asked the server for (and decode).
    previewed: Vec<String>,
    previews_visible: bool,
    preview_first: usize,
    share_preview_route: Arc<Mutex<Option<SyncSender<VideoOut>>>>,
    watch_layer: WatchLayer,
    layer: Option<LayerState>,
    watching: Option<String>,
    viewer: Option<Viewer>,
    /// SSRC of the watched share (0 = none), checked on the call thread.
    watched_ssrc: Arc<AtomicU32>,
    viewer_route: Arc<Mutex<Option<SyncSender<VideoIn>>>>,
    keyframes: Arc<Mutex<Option<KeyframeRequester>>>,
    picture: PictureSlot,
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
            auth_token: String::new(),
            authorization: None,
            auth_changed: false,
            pending_leaves: Vec::new(),
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
            share: None,
            share_route: Arc::default(),
            system_audio: None,
            share_audio_route: Arc::default(),
            screen_audio_tracks: HashMap::new(),
            watched_audio: None,
            screen_tracks: HashMap::new(),
            shares: HashMap::new(),
            preview_tracks: HashMap::new(),
            preview_decoder: None,
            preview_ssrcs: Arc::default(),
            preview_route: Arc::default(),
            preview_slots: PreviewSlots::default(),
            previewed: Vec::new(),
            previews_visible: true,
            preview_first: 0,
            share_preview_route: Arc::default(),
            watch_layer: WatchLayer::Auto,
            layer: None,
            watching: None,
            viewer: None,
            watched_ssrc: Arc::default(),
            viewer_route: Arc::default(),
            keyframes: Arc::default(),
            picture: PictureSlot::default(),
        }
    }

    async fn run(&mut self, mut cmds: mpsc::Receiver<Command>) {
        let mut call_rx = self.call_rx.take().expect("run once");
        let mut tunnel_rx = self.tunnel_rx.take().expect("run once");
        let mut backoff = Duration::from_millis(500);
        while !self.quit {
            self.flush_leaves().await;
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
        self.stop_share();
        self.flush_leaves().await;
        self.unwatch();
        self.stop_audio();
        self.snap.server = ServerStatus::Disconnected("вышли".into());
        self.publish();
    }

    /// WebSocket + Hello/Welcome. Returns the heartbeat interval.
    async fn flush_leaves(&mut self) {
        for (call, participation, token) in std::mem::take(&mut self.pending_leaves) {
            let base = self
                .cfg
                .server_url
                .trim_end_matches("/v1/signal")
                .replacen("ws://", "http://", 1)
                .replacen("wss://", "https://", 1);
            let mut api = turbo_pancake_messenger_client::api::Api::with_tls(
                &base,
                self.cfg
                    .tls
                    .clone()
                    .unwrap_or_else(turbo_pancake_messenger_client::api::web_pki),
            );
            api.set_token(Some(token));
            // Bounded cleanup; server-side lease expiry repairs network loss.
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                api.leave_call(&call, &participation),
            )
            .await;
        }
    }

    async fn connect(&mut self) -> Result<(Ws, Duration), String> {
        self.auth_changed = false;
        let (mut ws, _) = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async_tls_with_config(
                self.cfg.server_url.as_str(),
                None,
                false,
                Some(tokio_tungstenite::Connector::Rustls(
                    self.cfg
                        .tls
                        .clone()
                        .unwrap_or_else(turbo_pancake_messenger_client::api::web_pki),
                )),
            ),
        )
        .await
        .map_err(|_| "таймаут подключения".to_string())?
        .map_err(|e| {
            if turbo_pancake_messenger_client::api::describe(&e)
                == turbo_pancake_messenger_client::api::UNTRUSTED_CERTIFICATE
            {
                "сертификат сервера не доверенный (проверьте --server-ca)".to_string()
            } else {
                e.to_string()
            }
        })?;
        self.dialed_ip = match ws.get_ref() {
            MaybeTlsStream::Plain(tcp) => tcp.peer_addr().ok().map(|a| a.ip()),
            MaybeTlsStream::Rustls(tls) => tls.get_ref().0.peer_addr().ok().map(|a| a.ip()),
            _ => None,
        };
        let hello = C::Hello(v1::Hello {
            client_version: self.cfg.client_version.clone(),
            display_name: self.cfg.display_name.clone(),
            auth_token: self.auth_token.clone(),
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
                    Some(S::Error(e)) => {
                        if e.code == v1::ErrorCode::Unauthenticated as i32
                            && !self.auth_token.is_empty()
                        {
                            self.leave_room();
                            self.want_room = None;
                            self.stop_audio();
                            self.snap.last_error = Some("Сессия завершена. Войдите снова.".into());
                        }
                        return Err(format!("сервер: {}", e.message));
                    }
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
            self.flush_leaves().await;
            for frame in self.outbox.drain(..) {
                if let Err(e) = sink.send(Message::Binary(frame.into())).await {
                    return e.to_string();
                }
            }
            if self.quit {
                let _ = sink.send(Message::Close(None)).await;
                return "вышли".into();
            }
            if self.auth_changed {
                return "обновление авторизации".into();
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
        self.screen_tracks.clear();
        self.screen_audio_tracks.clear();
        self.shares.clear();
        // Peer ids are per connection (accounts arrive in S5): the volumes and the
        // watched share cannot follow. Our own share keeps running and is announced again.
        self.unwatch();
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
            self.join_rid = if let Some(p) = &self.authorization {
                self.push(C::JoinCallMedia(v1::JoinCallMedia {
                    call_id: p.call.as_ref().unwrap().call_id.clone(),
                    participation_id: p.participation_id.clone(),
                }))
            } else {
                self.push(C::JoinRoom(v1::JoinRoom { room_id: room }))
            };
        }
    }

    fn command(&mut self, cmd: Command) {
        self.dirty = true;
        match cmd {
            Command::Reconnect => self.auth_changed = true,
            Command::JoinConversation {
                token,
                participation,
            } => {
                let Some(call) = &participation.call else {
                    return;
                };
                let room = format!("call_{}", call.call_id);
                if self
                    .authorization
                    .as_ref()
                    .is_some_and(|p| p.participation_id == participation.participation_id)
                {
                    return;
                }
                self.leave_room();
                self.auth_changed = self.auth_token != token;
                self.auth_token = token;
                self.authorization = Some(participation);
                self.snap.last_error = None;
                self.want_room = Some(room.clone());
                if self.audio.is_none() {
                    self.start_audio();
                }
                if self.audio.is_none() {
                    self.leave_room();
                    self.want_room = None;
                    return;
                }
                if !self.auth_changed {
                    self.send_join(room);
                }
            }
            Command::SignOut => {
                self.command(Command::Leave);
                self.auth_token.clear();
                self.auth_changed = true;
            }
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
            Command::StartShare(config) => self.start_share(config),
            Command::StopShare => self.stop_share(),
            Command::SetShareAudio(on) => {
                self.cfg.share_audio = on;
                if self.share.is_some() {
                    self.sync_system_audio();
                }
            }
            Command::Watch(peer) => self.watch(peer),
            Command::SetWatchLayer(layer) => {
                self.watch_layer = layer;
                if self.watching.is_some() {
                    self.subscribe();
                }
            }
            Command::SetPreviews { visible, first } => {
                self.previews_visible = visible;
                self.preview_first = first;
                self.sync_previews();
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

    /// Starts (or replaces) our share; announced to the room with the next chance.
    fn start_share(&mut self, config: ShareConfig) {
        if self.share.is_some() {
            // Switching monitor or quality: a new pipeline, the old one stops first.
            if let Some(old) = self.share.take() {
                old.share.stop();
            }
        }
        let route = self.share_route.clone();
        let deliver = Box::new(move |f: turbo_pancake_media_video::EncodedFrame| {
            let route = route.lock().unwrap();
            let Some(tx) = route.as_ref() else {
                return false;
            };
            tx.try_send(VideoOut {
                timestamp: rtp_time(f.timestamp),
                data: f.data,
            })
            .is_ok()
        });
        let preview_route = self.share_preview_route.clone();
        let preview = Box::new(move |f: turbo_pancake_media_video::EncodedFrame| {
            let route = preview_route.lock().unwrap();
            let Some(tx) = route.as_ref() else {
                return false;
            };
            tx.try_send(VideoOut {
                timestamp: rtp_time(f.timestamp),
                data: f.data,
            })
            .is_ok()
        });
        match ScreenShare::start_layers(config, deliver, Some(preview)) {
            Ok(share) => {
                if let Some(m) = &self.media {
                    let _ = m.driver.set_video_pacing(pacing_bps(config.bitrate_kbps));
                }
                self.share = Some(Sharing {
                    share,
                    config,
                    adapter: None,
                    last_adapt: Instant::now(),
                    pacing_drops: self
                        .media
                        .as_ref()
                        .map_or(0, |m| m.driver.stats().video_frames_dropped_pacing),
                    announced: (0, 0, 0, 0),
                    nacks: (0, 0),
                });
                self.snap.last_error = None;
                self.sync_system_audio();
                self.announce_share();
            }
            Err(VideoError::Unsupported) => {
                self.snap.last_error = Some("показ экрана на этой ОС пока не реализован".into());
            }
            Err(e) => self.snap.last_error = Some(format!("показ экрана: {e}")),
        }
    }

    fn stop_share(&mut self) {
        if let Some(s) = self.share.take() {
            s.share.stop();
            self.sync_system_audio();
            self.announce_share();
        }
    }

    /// Runs the system audio capture exactly while we share and it is wanted.
    fn sync_system_audio(&mut self) {
        let want = self.share.is_some() && self.cfg.share_audio;
        if !want {
            if let Some(c) = self.system_audio.take() {
                c.stop();
            }
            return;
        }
        if self.system_audio.is_some() {
            return;
        }
        let route = self.share_audio_route.clone();
        let started = SystemAudioCapture::start(move |f| {
            if let Some(tx) = route.lock().unwrap().as_ref() {
                // Full: the call thread is behind; drop like a lost packet.
                let _ = tx.try_send(AudioOut {
                    seq: f.seq,
                    timestamp: f.timestamp,
                    payload: f.payload,
                });
            }
        });
        match started {
            Ok(c) => self.system_audio = Some(c),
            // The share goes on without sound; the UI says why (spec §5.1: no mixed
            // capture with our own sound as a fallback).
            Err(e) => self.snap.last_error = Some(format!("звук компьютера: {e}")),
        }
    }

    /// Tells the server whether we share (needs the media session; repeated after every
    /// new media offer).
    fn announce_share(&mut self) {
        let (Some(room), true) = (self.joined.clone(), self.media.is_some()) else {
            return;
        };
        let body = match &mut self.share {
            Some(s) => {
                let st = s.share.stats();
                // Before the first stats the configured values (fitted later).
                let (w, h, fps, kbps) = if st.width > 0 {
                    (st.width, st.height, st.fps, st.bitrate_kbps_target)
                } else {
                    (
                        s.config.width,
                        s.config.height,
                        s.config.fps,
                        s.config.bitrate_kbps,
                    )
                };
                s.announced = (w, h, fps, kbps);
                v1::ScreenShare {
                    room_id: room,
                    active: true,
                    width: w,
                    height: h,
                    fps,
                    bitrate_kbps: kbps,
                    preview: s.config.preview.is_some(),
                }
            }
            None => v1::ScreenShare {
                room_id: room,
                ..Default::default()
            },
        };
        self.push(C::ScreenShare(body));
    }

    fn watch(&mut self, peer: Option<String>) {
        let Some(peer) = peer else {
            self.unwatch();
            self.subscribe();
            self.sync_previews();
            return;
        };
        if self.viewer.is_none() {
            match Viewer::start(self.picture.clone(), self.keyframes.clone()) {
                Ok(v) => {
                    *self.viewer_route.lock().unwrap() = Some(v.sender());
                    self.viewer = Some(v);
                }
                Err(e) => {
                    self.snap.last_error = Some(format!("просмотр: {e}"));
                    return;
                }
            }
        }
        if self.watching.as_ref() != Some(&peer) {
            self.layer = None;
        }
        self.watching = Some(peer);
        self.sync_watch();
        self.subscribe();
        self.sync_previews();
    }

    /// Stops watching: the viewer thread ends and the picture goes away.
    fn unwatch(&mut self) {
        self.watching = None;
        self.layer = None;
        if let Some(old) = self.watched_audio.take()
            && let Some(a) = &self.audio
        {
            a.accepted.lock().unwrap().remove(&old);
            let _ = a.engine.remove_peer(PeerId::from(old));
        }
        self.watched_ssrc.store(0, Ordering::Relaxed);
        *self.keyframes.lock().unwrap() = None;
        *self.viewer_route.lock().unwrap() = None;
        if let Some(v) = self.viewer.take() {
            v.stop();
        }
        *self.picture.lock().unwrap() = None;
    }

    /// Points the call thread's video filter and the viewer at the watched share's stream.
    fn sync_watch(&mut self) {
        let track = self
            .watching
            .as_ref()
            .and_then(|p| self.screen_tracks.get(p).copied());
        let ssrc = track.map_or(0, |(s, _)| s);
        if self.watched_ssrc.swap(ssrc, Ordering::Relaxed) != ssrc
            && let Some(v) = &self.viewer
        {
            v.reset();
        }
        *self.keyframes.lock().unwrap() = match (&self.media, track) {
            (Some(m), Some((s, _))) => Some(m.driver.keyframe_requester(s)),
            _ => None,
        };
        // The watched share's sound plays as its own engine stream.
        let audio = self
            .watching
            .as_ref()
            .and_then(|p| self.screen_audio_tracks.get(p).copied());
        if self.watched_audio != audio {
            if let Some(old) = self.watched_audio.take()
                && let Some(a) = &self.audio
            {
                a.accepted.lock().unwrap().remove(&old);
                let _ = a.engine.remove_peer(PeerId::from(old));
            }
            if let (Some(ssrc), Some(a)) = (audio, &self.audio) {
                a.accepted.lock().unwrap().insert(ssrc);
                let _ = a
                    .engine
                    .set_peer_volume(PeerId::from(ssrc), self.remote_volume);
            }
            self.watched_audio = audio;
        }
    }

    /// Tells the server whose share we watch (needs the media session).
    fn subscribe(&mut self) {
        let (Some(room), true) = (self.joined.clone(), self.media.is_some()) else {
            return;
        };
        let peer_id = self.watching.clone().unwrap_or_default();
        let layer = match self.watch_layer {
            WatchLayer::Auto => v1::ScreenLayer::Auto,
            WatchLayer::High => v1::ScreenLayer::High,
            WatchLayer::Low => v1::ScreenLayer::Low,
        };
        self.push(C::SubscribeScreen(v1::SubscribeScreen {
            room_id: room,
            peer_id,
            layer: layer as i32,
        }));
    }

    /// The previews we want: the visible page of other members' shares (member order,
    /// the watched one left out: it is shown large). Tells the server and the decoder
    /// when it changed.
    fn sync_previews(&mut self) {
        let mut order: Vec<(u64, String)> = self
            .shares
            .iter()
            .filter(|(p, st)| st.preview && self.watching.as_deref() != Some(p.as_str()))
            .filter_map(|(p, _)| self.members.get(p).map(|(o, _)| (*o, p.clone())))
            .collect();
        order.sort();
        let want: Vec<String> = if self.previews_visible {
            order
                .into_iter()
                .map(|(_, p)| p)
                .skip(self.preview_first)
                .take(self.cfg.max_previews)
                .collect()
        } else {
            Vec::new()
        };
        // The decoder follows what we receive (streams the server announced).
        let streams: Vec<(u32, Option<KeyframeRequester>)> = want
            .iter()
            .filter_map(|p| self.preview_tracks.get(p))
            .map(|(ssrc, _)| {
                (
                    *ssrc,
                    self.media
                        .as_ref()
                        .map(|m| m.driver.keyframe_requester(*ssrc)),
                )
            })
            .collect();
        *self.preview_ssrcs.lock().unwrap() = streams.iter().map(|(s, _)| *s).collect();
        if streams.is_empty() {
            *self.preview_route.lock().unwrap() = None;
            if let Some(d) = self.preview_decoder.take() {
                d.stop();
            }
            self.preview_slots.lock().unwrap().clear();
        } else {
            if self.preview_decoder.is_none() {
                match PreviewDecoder::start(self.preview_slots.clone()) {
                    Ok(d) => {
                        *self.preview_route.lock().unwrap() = Some(d.sender());
                        self.preview_decoder = Some(d);
                    }
                    Err(e) => self.snap.last_error = Some(format!("превью: {e}")),
                }
            }
            if let Some(d) = &self.preview_decoder {
                d.set_streams(streams);
            }
        }
        if want != self.previewed
            && let (Some(room), true) = (self.joined.clone(), self.media.is_some())
        {
            self.previewed = want.clone();
            self.push(C::SubscribePreviews(v1::SubscribePreviews {
                room_id: room,
                peer_ids: want,
            }));
        }
    }

    /// Once a second while sharing: adapts the high layer to what the uplink reports.
    fn adapt_share(&mut self) {
        let Some(stats) = self.media.as_ref().map(|m| m.driver.stats()) else {
            return;
        };
        let Some(s) = self.share.as_mut() else {
            return;
        };
        if !s.config.adaptive || s.last_adapt.elapsed() < Duration::from_secs(1) {
            return;
        }
        s.last_adapt = Instant::now();
        let st = s.share.stats();
        if st.width == 0 {
            return;
        }
        let adapter = s.adapter.get_or_insert_with(|| {
            Adapter::new(
                (st.source_width, st.source_height),
                Rung {
                    width: st.width,
                    height: st.height,
                    fps: st.fps,
                },
                s.config.bitrate_kbps,
            )
        });
        let drops = stats.video_frames_dropped_pacing;
        let dropped = drops > s.pacing_drops;
        s.pacing_drops = drops;
        // Receiver reports count repaired packets as received, so with retransmission
        // they show little of a lossy uplink: the share of packets the server asked to
        // repeat (NACKs) is the other loss signal.
        let (n0, p0) = s.nacks;
        let (n1, p1) = (stats.video_nacks_received, stats.video_packets_sent);
        s.nacks = (n1, p1);
        // Only over enough packets: at low rates one NACK per repair round is a lot.
        let nack_ratio =
            (p1 >= p0 + 100 && n1 >= n0 && p0 > 0).then(|| (n1 - n0) as f32 / (p1 - p0) as f32);
        let loss = match (stats.video_egress_loss, nack_ratio) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        let Some(t) = adapter.update(Instant::now(), loss, dropped) else {
            return;
        };
        if (t.rung.width, t.rung.height, t.rung.fps) != (st.width, st.height, st.fps) {
            s.share.reconfigure(t.rung.width, t.rung.height, t.rung.fps);
        }
        s.share.set_bitrate(t.bitrate_kbps);
        if let Some(m) = &self.media {
            let _ = m.driver.set_video_pacing(pacing_bps(t.bitrate_kbps));
        }
    }

    /// Ends media and tells the server we left (also for a join still in flight).
    fn leave_room(&mut self) {
        if let Some(p) = self.authorization.take()
            && let Some(c) = p.call
        {
            self.pending_leaves
                .push((c.call_id, p.participation_id, self.auth_token.clone()));
        }
        self.end_media();
        self.stop_share();
        self.unwatch();
        self.screen_tracks.clear();
        self.screen_audio_tracks.clear();
        self.preview_tracks.clear();
        self.shares.clear();
        self.previewed.clear();
        self.layer = None;
        self.sync_previews();
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
                self.shares.remove(&l.peer_id);
                if self.watching.as_deref() == Some(l.peer_id.as_str()) {
                    self.unwatch();
                }
                self.sync_previews();
            }
            S::TrackPublished(t) if t.kind() == v1::TrackKind::ScreenPreview => {
                if self.joined.as_deref() != Some(t.room_id.as_str()) {
                    return;
                }
                let rtx = (t.rtx_ssrc != 0).then_some(t.rtx_ssrc);
                if let Some((old, _)) = self.preview_tracks.insert(t.peer_id.clone(), (t.ssrc, rtx))
                    && old != t.ssrc
                    && let Some(m) = &self.media
                {
                    let _ = m.driver.forget_source(old);
                }
                if let Some(m) = &self.media {
                    let _ = m.driver.expect_preview_source(t.ssrc, rtx);
                }
                self.sync_previews();
            }
            S::TrackUnpublished(t) if t.kind() == v1::TrackKind::ScreenPreview => {
                if self.preview_tracks.get(&t.peer_id).map(|(s, _)| *s) == Some(t.ssrc) {
                    self.preview_tracks.remove(&t.peer_id);
                    if let Some(m) = &self.media {
                        let _ = m.driver.forget_source(t.ssrc);
                    }
                    self.sync_previews();
                }
            }
            S::ScreenSubscription(sub) => {
                if self.watching.as_deref() == Some(sub.peer_id.as_str()) {
                    self.layer = Some(LayerState {
                        high: sub.layer() == v1::ScreenLayer::High,
                        reason: sub.reason(),
                    });
                }
            }
            S::TrackPublished(t) if t.kind() == v1::TrackKind::Screen => {
                if self.joined.as_deref() != Some(t.room_id.as_str()) {
                    return;
                }
                let rtx = (t.rtx_ssrc != 0).then_some(t.rtx_ssrc);
                if let Some((old, _)) = self.screen_tracks.insert(t.peer_id.clone(), (t.ssrc, rtx))
                    && old != t.ssrc
                    && let Some(m) = &self.media
                {
                    let _ = m.driver.forget_source(old);
                }
                if let Some(m) = &self.media {
                    let _ = m.driver.expect_video_source(t.ssrc, rtx);
                }
                if self.watching.as_deref() == Some(t.peer_id.as_str()) {
                    self.sync_watch();
                }
            }
            S::TrackPublished(t) if t.kind() == v1::TrackKind::ScreenAudio => {
                if self.joined.as_deref() != Some(t.room_id.as_str()) {
                    return;
                }
                if let Some(old) = self.screen_audio_tracks.insert(t.peer_id.clone(), t.ssrc)
                    && old != t.ssrc
                    && let Some(m) = &self.media
                {
                    let _ = m.driver.forget_source(old);
                }
                if let Some(m) = &self.media {
                    let _ = m.driver.expect_screen_audio_source(t.ssrc);
                }
                if self.watching.as_deref() == Some(t.peer_id.as_str()) {
                    self.sync_watch();
                }
            }
            S::TrackUnpublished(t) if t.kind() == v1::TrackKind::ScreenAudio => {
                if self.screen_audio_tracks.get(&t.peer_id) == Some(&t.ssrc) {
                    self.screen_audio_tracks.remove(&t.peer_id);
                    if let Some(m) = &self.media {
                        let _ = m.driver.forget_source(t.ssrc);
                    }
                    if self.watching.as_deref() == Some(t.peer_id.as_str()) {
                        self.sync_watch();
                    }
                }
            }
            S::TrackUnpublished(t) if t.kind() == v1::TrackKind::Screen => {
                if self.screen_tracks.get(&t.peer_id).map(|(s, _)| *s) == Some(t.ssrc) {
                    self.screen_tracks.remove(&t.peer_id);
                    if let Some(m) = &self.media {
                        let _ = m.driver.forget_source(t.ssrc);
                    }
                    if self.watching.as_deref() == Some(t.peer_id.as_str()) {
                        self.sync_watch();
                    }
                }
            }
            S::ScreenShareState(s) => {
                if self.joined.as_deref() != Some(s.room_id.as_str()) {
                    return;
                }
                if s.active {
                    self.shares.insert(
                        s.peer_id,
                        ShareState {
                            width: s.width,
                            height: s.height,
                            fps: s.fps,
                            bitrate_kbps: s.bitrate_kbps,
                            preview: s.preview,
                        },
                    );
                } else {
                    self.shares.remove(&s.peer_id);
                    if self.watching.as_deref() == Some(s.peer_id.as_str()) {
                        self.unwatch();
                    }
                }
                self.sync_previews();
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
                    if let Some(p) = self.authorization.take()
                        && let Some(c) = p.call
                    {
                        self.pending_leaves.push((
                            c.call_id,
                            p.participation_id,
                            self.auth_token.clone(),
                        ));
                    }
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
            // Our own leave or switch is already done locally (`joined` was cleared
            // first). A RoomLeft for the room we are in means the server ended our
            // membership: the call is over, do not rejoin on our own.
            S::RoomLeft(r) if self.joined.as_deref() == Some(r.room_id.as_str()) => {
                self.leave_room();
                self.want_room = None;
                self.stop_audio();
                self.snap.last_error = Some(if r.room_id.starts_with("call_") {
                    "Звонок завершён сервером".into()
                } else {
                    format!("звонок в «{}» завершён сервером", r.room_id)
                });
            }
            // Welcome, relayed signals; messenger events go to the messenger's own
            // connection (crates/messenger-client).
            _ => {}
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
                // The server processes these after the offer: a new session learns our
                // share and our subscription again.
                if self.share.is_some() {
                    self.announce_share();
                }
                if self.watching.is_some() {
                    self.subscribe();
                }
                // A new session receives no previews until asked again.
                self.previewed.clear();
                self.sync_previews();
            }
            // The SFU is ICE-lite with its candidates in the answer: nothing to trickle.
            CallEvent::Signal(_) => {}
            CallEvent::KeyframeRequested => self.keyframe_requested(),
            CallEvent::PreviewKeyframeRequested => {
                if let Some(s) = &self.share {
                    s.share.request_preview_keyframe();
                }
            }
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

    /// The server asked for a keyframe of our share (a new viewer, a loss).
    fn keyframe_requested(&mut self) {
        if let Some(s) = &self.share {
            s.share.request_keyframe();
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
        cc.recv_loss = self.cfg.recv_loss;
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
                // Audio + RTCP from the server, and bursts of a watched share's keyframes.
                let (in_tx, in_rx) = sync_channel::<Vec<u8>>(2048);
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
        let (video_tx, video_rx) = sync_channel::<VideoOut>(16);
        let (preview_tx, preview_rx) = sync_channel::<VideoOut>(8);
        let (preview_ssrcs, preview_route) =
            (self.preview_ssrcs.clone(), self.preview_route.clone());
        let (share_audio_tx, share_audio_rx) = sync_channel::<AudioOut>(50);
        let (watched, viewer) = (self.watched_ssrc.clone(), self.viewer_route.clone());
        let video = VideoIo {
            audio_out: share_audio_rx,
            outgoing: video_rx,
            preview: preview_rx,
            on_video: Box::new(move |p: VideoIn| {
                if p.preview {
                    if preview_ssrcs.lock().unwrap().contains(&p.ssrc)
                        && let Some(tx) = preview_route.lock().unwrap().as_ref()
                    {
                        // Full: the decoder is behind; drop like a lost packet.
                        let _ = tx.try_send(p);
                    }
                    return;
                }
                if p.ssrc != watched.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(tx) = viewer.lock().unwrap().as_ref() {
                    // Full: the viewer is behind; drop like a lost packet.
                    let _ = tx.try_send(p);
                }
            }),
        };
        let on_audio = move |p: turbo_pancake_media_transport::call::AudioIn| {
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
        };
        let started = CallDriver::start_media(cc, link, rx, on_audio, Some(video));
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
        *self.share_route.lock().unwrap() = Some(video_tx);
        *self.share_preview_route.lock().unwrap() = Some(preview_tx);
        *self.share_audio_route.lock().unwrap() = Some(share_audio_tx);
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
        if let Some(m) = &self.media {
            for (ssrc, rtx) in self.screen_tracks.values() {
                let _ = m.driver.expect_video_source(*ssrc, *rtx);
            }
            for ssrc in self.screen_audio_tracks.values() {
                let _ = m.driver.expect_screen_audio_source(*ssrc);
            }
            for (ssrc, rtx) in self.preview_tracks.values() {
                let _ = m.driver.expect_preview_source(*ssrc, *rtx);
            }
            if let Some(s) = &self.share {
                let kbps = s
                    .adapter
                    .as_ref()
                    .map_or(s.config.bitrate_kbps, |a| a.target().bitrate_kbps);
                let _ = m.driver.set_video_pacing(pacing_bps(kbps));
            }
        }
        self.sync_watch();
    }

    fn end_media(&mut self) {
        let Some(media) = self.media.take() else {
            return;
        };
        if let Some(a) = &self.audio {
            *a.route.lock().unwrap() = None;
        }
        *self.share_route.lock().unwrap() = None;
        *self.share_preview_route.lock().unwrap() = None;
        *self.share_audio_route.lock().unwrap() = None;
        self.layer = None;
        if let (Some(s), Some(a)) = (self.watched_audio.take(), &self.audio) {
            let _ = a.engine.remove_peer(PeerId::from(s));
        }
        *self.keyframes.lock().unwrap() = None;
        // The next session's streams start over.
        self.watched_ssrc.store(0, Ordering::Relaxed);
        if let Some(v) = &self.viewer {
            v.reset();
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
        // A share that stopped on its own (monitor resolution changed, device lost).
        if let Some(s) = &self.share
            && !s.share.is_running()
        {
            let why = s.share.stats().error.unwrap_or_else(|| "остановлен".into());
            self.snap.last_error = Some(format!("показ экрана: {why}"));
            self.stop_share();
        }
        self.adapt_share();
        // The size, rate or bitrate the server knows is behind (adaptation, fitting).
        if let Some(s) = &self.share {
            let st = s.share.stats();
            let now = (st.width, st.height, st.fps, st.bitrate_kbps_target);
            if st.width > 0 && now != s.announced {
                self.announce_share();
            }
        }
        self.snap.sharing = self.share.as_ref().map(|s| s.share.stats());
        self.snap.watch_layer = self.watch_layer;
        self.snap.layer = self.layer.clone();
        self.snap.previews_visible = self.previews_visible;
        self.snap.preview_first = self.preview_first;
        self.snap.max_previews = self.cfg.max_previews;
        self.snap.previews = self.preview_decoder.as_ref().map(PreviewDecoder::stats);
        self.snap.share_audio = self.system_audio.as_ref().map(SystemAudioCapture::stats);
        self.snap.share_config = self.share.as_ref().map(|s| s.config);
        self.snap.watching = self.watching.clone();
        self.snap.viewer = self.viewer.as_ref().map(Viewer::stats);
        let mut shares: Vec<(u64, ShareView)> = self
            .shares
            .iter()
            .filter_map(|(peer, st)| {
                let (order, name) = self.members.get(peer)?;
                Some((
                    *order,
                    ShareView {
                        peer_id: peer.clone(),
                        name: name.clone(),
                        width: st.width,
                        height: st.height,
                        fps: st.fps,
                        bitrate_kbps: st.bitrate_kbps,
                        has_preview: st.preview,
                        preview_ssrc: self
                            .previewed
                            .contains(peer)
                            .then(|| self.preview_tracks.get(peer).map(|(s, _)| *s))
                            .flatten(),
                    },
                ))
            })
            .collect();
        shares.sort_by_key(|(o, _)| *o);
        self.snap.shares = shares.into_iter().map(|(_, s)| s).collect();
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
        self.snap.watched_audio = self
            .watched_audio
            .and_then(|s| streams?.iter().find(|p| p.peer == PeerId::from(s)).cloned());
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
