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

mod events;
mod media;
mod share;
mod watch;

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
    LossAdapter, PacketSink, PeerId, PeerStatus, VoiceLink,
};
use turbo_pancake_media_transport::call::{
    AudioOut, CallConfig, CallDriver, CallEvent, CallState, CallStats, KeyframeRequester,
    LocalSignal, MediaLink, Role, VideoIo,
};
use turbo_pancake_media_transport::video::{VideoIn, VideoOut};
use turbo_pancake_media_video::adapt::{Adapter, Rung};
use turbo_pancake_media_video::{ScreenShare, ShareConfig, ShareStats, VideoError};

use crate::screen::{
    PictureSlot, PresentTarget, PreviewDecoder, PreviewSlots, PreviewStats, Viewer, ViewerStats,
    rtp_time,
};
use turbo_pancake_protocol::{self as proto, v1};
use v1::{client_envelope::Body as C, server_envelope::Body as S};

use share::RestartBudget;

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
    present: Arc<PresentTarget>,
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
        let present = Arc::new(PresentTarget::default());
        let core_present = present.clone();
        let previews = PreviewSlots::default();
        let core_previews = previews.clone();
        let thread = std::thread::Builder::new()
            .name("tp-session".into())
            .spawn(move || {
                let mut core = Core::new(config, start_engine, Box::new(on_update));
                core.picture = core_picture;
                core.present = core_present;
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
            present,
            previews,
            thread: Some(thread),
        })
    }

    /// The newest preview pictures, by preview SSRC (`ShareView::preview_ssrc`).
    pub fn previews(&self) -> PreviewSlots {
        self.previews.clone()
    }

    /// Where the watched share may be drawn from the GPU (the UI names the window).
    pub fn present_target(&self) -> Arc<PresentTarget> {
        self.present.clone()
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
    /// Automatic restarts of our share after its pipeline stopped on its own.
    share_restarts: RestartBudget,
    /// When to start the share again, and with what.
    share_retry: Option<(Instant, ShareConfig)>,
    /// Worst loss on the way from the server to the other members (`VoiceFeedback`).
    listeners_loss: f32,
    /// Voice encoder resilience following the measured loss.
    voice_adapter: LossAdapter,
    voice_adapt_at: Instant,
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
    present: Arc<PresentTarget>,
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
            share_restarts: RestartBudget::default(),
            share_retry: None,
            listeners_loss: 0.0,
            voice_adapter: LossAdapter::new(cfg.engine.encoder),
            voice_adapt_at: Instant::now(),
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
            present: Arc::default(),
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
            Command::StartShare(config) => {
                self.share_restarts = RestartBudget::default();
                self.share_retry = None;
                self.start_share(config)
            }
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

    fn publish(&mut self) {
        // A share that stopped on its own (monitor resolution changed, device lost):
        // the pipeline is rebuilt for the monitor as it is now, a few times at most.
        if let Some(s) = &self.share
            && !s.share.is_running()
        {
            let why = s.share.stats().error.unwrap_or_else(|| "остановлен".into());
            let config = s.config;
            self.stop_share();
            self.retry_share(config, &why);
        }
        if let Some((at, config)) = self.share_retry
            && Instant::now() >= at
        {
            self.share_retry = None;
            self.start_share(config);
            if self.share.is_none() {
                let why = self.snap.last_error.clone().unwrap_or_default();
                self.retry_share(config, &why);
            }
        }
        self.adapt_share();
        self.adapt_voice();
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
