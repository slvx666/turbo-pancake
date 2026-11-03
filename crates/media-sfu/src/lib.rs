//! Selective forwarding unit (spec §3 `media-sfu`, §6): every room member negotiates one
//! WebRTC session (ICE-lite + DTLS-SRTP, Opus, RTP mode) with the server, and RTP from one
//! member is forwarded to the others without transcoding.
//!
//! ```text
//! signaling (async) ── bounded commands ──▶ sfu thread ◀──▶ one UDP socket (all UDP members)
//!                    ◀── PeerSink (answer, tunneled datagrams; never blocks)
//! ```
//!
//! Members reach the server, never each other, so calls work behind any NAT: a member
//! always sends first and the server answers to the address the datagrams came from. When
//! UDP does not get through (VPNs, firewalls) the same encrypted datagrams travel inside
//! the signaling WebSocket ([`Link::Tunnel`]).
//!
//! Every member hears every other member (ADR 0008): one audio m-line per session, and
//! each member's audio leaves the SFU as its own RTP stream with an SSRC the SFU assigns
//! for that member's session. [`PeerSink::track`] tells a member which SSRC belongs to
//! whom; no renegotiation happens when members come and go. The RTP `mid` header
//! extension is not negotiated: with several SSRCs on one m-line a receiver would use it
//! to re-map its existing stream to an unknown SSRC.
//!
//! Screen share (S3): every session also has one H.264 video m-line. A member's share
//! leaves the SFU as its own stream (SSRC + RTX SSRC assigned per session, announced like
//! audio), but only to members that subscribed to it ([`Sfu::subscribe`]) and only while
//! the member shares ([`Sfu::share`]). Losses are repaired hop by hop: str0m answers NACKs
//! from its send buffer on each leg (RTX), and keyframe requests (PLI/FIR) from a viewer go
//! to the sharer. The share's system audio has a third m-line (Opus) and is forwarded the
//! same way: only to the share's subscribers.
//!
//! Layers (S4, ADR 0011): a session may offer a second video m-line on which the member
//! sends a low preview layer of its share (≈ 320 px, ≈ 10 fps). Previews go to the members
//! that list the sharer in [`Sfu::previews`] (as one more stream each, on their own second
//! video m-line). A viewer's watched (main) stream carries either layer ([`layer`]): what
//! it asked for, the low one when its downlink loses packets (auto) or when the server's
//! budget for high streams ([`SfuConfig::max_high_egress_kbps`]) is used up. A layer switch
//! waits for a keyframe of the new layer and continues the outgoing sequence numbers, so
//! the viewer's decoder sees one stream that changes size. Both layers carry the sharer's
//! capture clock as RTP timestamps.

pub mod layer;

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use str0m::change::SdpOffer;
use str0m::format::Codec;
use str0m::media::{KeyframeRequestKind, MediaKind, Mid, Pt, Rid};
use str0m::net::{Protocol, Receive};
use str0m::rtp::{Extension, ExtensionMap, RtpWrite, SeqNo, Ssrc};
use str0m::{Candidate, Event, Input, Output, Rtc};
use turbo_pancake_media_transport::video::starts_keyframe;
use turbo_pancake_media_transport::{TUNNEL_CLIENT, TUNNEL_SERVER, init_crypto};

pub use layer::{Layer, LayerChoice, LayerReason, NetworkPolicy};

pub type PeerId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Audio,
    /// Screen share video (H.264) on the video m-line: the watched (main) stream.
    Screen,
    /// The share's system audio (Opus) on the screen audio m-line.
    ScreenAudio,
    /// The share's low layer on the preview (second video) m-line.
    ScreenPreview,
}

/// One member's stream as it appears on another member's session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Track {
    pub kind: TrackKind,
    pub ssrc: u32,
    /// Retransmission stream (video only).
    pub rtx: Option<u32>,
}

/// What a member shares (informational for viewers).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShareInfo {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Target bitrate of the high layer (0 = unknown).
    pub bitrate_kbps: u32,
    /// A preview layer is sent (and the session can carry it).
    pub preview: bool,
}

/// The layer a viewer receives for its watched share, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    pub source: PeerId,
    pub requested: LayerChoice,
    /// Flowing now, or after the pending switch.
    pub layer: Layer,
    pub reason: LayerReason,
}

/// Where the SFU delivers what belongs to one member. Called on the SFU thread: must not
/// block (drop datagrams when the member's connection is full).
pub trait PeerSink: Send + 'static {
    /// The SDP answer to the member's offer, or why it was rejected.
    fn answer(&mut self, result: Result<String, String>);
    /// One datagram for a [`Link::Tunnel`] member.
    fn datagram(&mut self, data: Vec<u8>);
    /// Another member's stream is published on this member's session, or that stream
    /// ended (`published == false`).
    fn track(&mut self, peer: &str, track: Track, published: bool);
    /// Another member started, changed (`Some`) or stopped (`None`) sharing its screen.
    fn screen(&mut self, peer: &str, share: Option<ShareInfo>);
    /// The layer of this member's watched share (after a subscribe and on every change).
    fn subscription(&mut self, _sub: &Subscription) {}
}

/// How a member's datagrams travel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// UDP to the SFU socket. `candidate_ips` are announced with the SFU port; the first
    /// one should be the address the member dialed (behind a port forward: the public IP).
    Udp { candidate_ips: Vec<IpAddr> },
    /// Inside the member's signaling connection.
    Tunnel,
}

#[derive(Debug, Clone, Default)]
pub struct SfuConfig {
    /// UDP socket for media; `None` = tunnel only.
    pub udp_bind: Option<SocketAddr>,
    /// Budget for high-layer streams to viewers (sum of the watched shares' announced
    /// bitrates, spec §5.3 admission control); `None` = unlimited. Viewers over the budget
    /// get the low layer (earlier subscriptions keep theirs).
    pub max_high_egress_kbps: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SfuStats {
    pub peers: usize,
    pub tunnel_peers: usize,
    pub connected_peers: usize,
    pub rtp_in: u64,
    pub rtp_out: u64,
    /// Screen share RTP (both layers) received / forwarded (included in `rtp_in` / `rtp_out`).
    pub video_in: u64,
    pub video_out: u64,
    /// Preview layer RTP received / forwarded to previews (included in `video_*`).
    pub preview_in: u64,
    pub preview_out: u64,
    /// Keyframe requests passed from viewers (or layer switches) to sharers.
    pub keyframe_requests: u64,
    /// Main streams that changed layer.
    pub layer_switches: u64,
    /// Viewers whose main stream is (or is about to be) the high / the low layer.
    pub viewers_high: usize,
    pub viewers_low: usize,
    /// Preview streams subscribed (to members that share).
    pub preview_subscriptions: usize,
    /// High-layer budget in use (announced bitrates), kbit/s.
    pub high_egress_kbps: u64,
    /// Outgoing per-member streams over all sessions (one per listener and source).
    pub streams_out: usize,
    pub udp_in: u64,
    pub udp_out: u64,
    /// UDP datagrams the OS refused to send (full buffer, unreachable, …).
    pub udp_send_errors: u64,
    pub tunnel_in: u64,
    pub tunnel_out: u64,
    /// Commands dropped because the queue was full.
    pub dropped_commands: u64,
}

enum Cmd {
    Join {
        peer: PeerId,
        room: String,
        offer: String,
        link: Link,
        sink: Box<dyn PeerSink>,
    },
    Leave(PeerId),
    Datagram(PeerId, Vec<u8>),
    Share(PeerId, Option<ShareInfo>),
    Subscribe(PeerId, Option<PeerId>, LayerChoice),
    Previews(PeerId, Vec<PeerId>),
    Stop,
}

/// Handle to the SFU thread; dropping it stops the thread.
pub struct Sfu {
    tx: SyncSender<Cmd>,
    udp_port: Option<u16>,
    stats: Arc<Mutex<SfuStats>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Sfu {
    /// Binds the UDP socket (if configured) and starts the thread.
    pub fn start(config: SfuConfig) -> std::io::Result<Sfu> {
        init_crypto();
        let socket = match config.udp_bind {
            Some(addr) => {
                let s = UdpSocket::bind(addr)?;
                s.set_nonblocking(true)?;
                turbo_pancake_media_transport::enlarge_buffers(&s);
                Some(s)
            }
            None => None,
        };
        let udp_port = socket
            .as_ref()
            .map(|s| s.local_addr().map(|a| a.port()))
            .transpose()?;
        // Tunnel datagrams of every member go through here: room for a few seconds of
        // audio from 100 members before anything is dropped.
        let (tx, rx) = sync_channel(8192);
        let stats = Arc::new(Mutex::new(SfuStats::default()));
        let worker = Worker {
            socket,
            udp_port: udp_port.unwrap_or(0),
            high_budget: config.max_high_egress_kbps,
            cmds: rx,
            peers: HashMap::new(),
            next_ssrc: first_ssrc(),
            next_order: 0,
            buf: vec![0; 2048],
            received: Collected::default(),
            st: SfuStats::default(),
            stats: stats.clone(),
        };
        let thread = std::thread::Builder::new()
            .name("tp-sfu".into())
            .spawn(move || worker.run())?;
        Ok(Sfu {
            tx,
            udp_port,
            stats,
            thread: Mutex::new(Some(thread)),
        })
    }

    pub fn udp_port(&self) -> Option<u16> {
        self.udp_port
    }

    fn send(&self, cmd: Cmd) -> bool {
        let ok = self.tx.try_send(cmd).is_ok();
        if !ok && let Ok(mut s) = self.stats.lock() {
            s.dropped_commands += 1;
        }
        ok
    }

    /// Starts (or replaces) the member's media session; the answer arrives through
    /// `sink`. `false` if the SFU is overloaded or stopped.
    pub fn join(
        &self,
        peer: &str,
        room: &str,
        offer: String,
        link: Link,
        sink: Box<dyn PeerSink>,
    ) -> bool {
        self.send(Cmd::Join {
            peer: peer.to_owned(),
            room: room.to_owned(),
            offer,
            link,
            sink,
        })
    }

    pub fn leave(&self, peer: &str) {
        self.send(Cmd::Leave(peer.to_owned()));
    }

    /// The member starts or updates (`Some`) or stops (`None`) sharing its screen; the
    /// others in its room are told. Ignored without a media session.
    pub fn share(&self, peer: &str, share: Option<ShareInfo>) -> bool {
        self.send(Cmd::Share(peer.to_owned(), share))
    }

    /// The member watches `source`'s share (`None` = none) with the high layer where
    /// possible ([`Sfu::subscribe_layer`] with [`LayerChoice::Auto`]).
    pub fn subscribe(&self, peer: &str, source: Option<&str>) -> bool {
        self.subscribe_layer(peer, source, LayerChoice::Auto)
    }

    /// The member watches `source`'s share (`None` = none) and asks for a layer. Replaces
    /// the previous subscription (the sharer is asked for a keyframe); the same source
    /// with another layer switches the layer of the running stream.
    pub fn subscribe_layer(&self, peer: &str, source: Option<&str>, layer: LayerChoice) -> bool {
        self.send(Cmd::Subscribe(
            peer.to_owned(),
            source.map(str::to_owned),
            layer,
        ))
    }

    /// The shares the member shows as previews (replaces the previous list).
    pub fn previews(&self, peer: &str, sources: Vec<PeerId>) -> bool {
        self.send(Cmd::Previews(peer.to_owned(), sources))
    }

    /// A datagram from a tunnel member. Dropped (like a lost packet) when the queue is full.
    pub fn datagram(&self, peer: &str, data: Vec<u8>) -> bool {
        self.send(Cmd::Datagram(peer.to_owned(), data))
    }

    pub fn stats(&self) -> SfuStats {
        self.stats.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn stop(&self) {
        if let Some(t) = self.thread.lock().ok().and_then(|mut t| t.take()) {
            // Blocking send: the thread drains the queue continuously.
            let _ = self.tx.send(Cmd::Stop);
            let _ = t.join();
        }
    }
}

impl Drop for Sfu {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The share a member watches and how its main stream is fed.
struct Watch {
    source: PeerId,
    requested: LayerChoice,
    /// Earlier subscriptions keep the high-layer budget first.
    order: u64,
    /// The layer being forwarded (`None` until the first packet went out).
    layer: Option<Layer>,
    /// The layer to switch to at its next keyframe.
    pending: Option<Layer>,
    /// Where the forwarded layer is going: target of the last decision.
    target: Layer,
    reason: LayerReason,
    /// Outgoing sequence number = source sequence number of the forwarded layer + offset.
    offset: i64,
    /// Highest outgoing sequence number written.
    last_out: Option<u64>,
    net: NetworkPolicy,
    /// What the member was told last.
    told: Option<(Layer, LayerReason)>,
}

impl Watch {
    fn new(source: PeerId, requested: LayerChoice, order: u64) -> Self {
        Self {
            source,
            requested,
            order,
            layer: None,
            pending: None,
            target: Layer::High,
            reason: LayerReason::Requested,
            offset: 0,
            last_out: None,
            net: NetworkPolicy::new(),
            told: None,
        }
    }
}

struct Member {
    room: String,
    rtc: Rtc,
    sink: Box<dyn PeerSink>,
    /// Local address stamped on received datagrams (the first announced candidate).
    local: SocketAddr,
    tunnel: bool,
    mid: Option<Mid>,
    pt: Option<Pt>,
    video_mid: Option<Mid>,
    /// Second video m-line (previews), when the session offered one.
    preview_mid: Option<Mid>,
    /// The offer had a second video m-line (known before str0m reports the m-lines).
    offers_preview: bool,
    /// H.264 payload types negotiated on this session.
    video_pts: Vec<Pt>,
    connected: bool,
    next_timeout: Instant,
    /// SSRC of this member's audio in every other member's session.
    ssrc: u32,
    /// SSRC and RTX SSRC of this member's screen share in every other member's session.
    video_ssrc: u32,
    video_rtx: u32,
    screen_audio_mid: Option<Mid>,
    /// SSRC of this member's share audio in every other member's session.
    screen_audio_ssrc: u32,
    /// SSRC and RTX SSRC of this member's preview layer in every other member's session.
    preview_ssrc: u32,
    preview_rtx: u32,
    /// The SSRCs this member's client sends its layers on (known after the first packet).
    video_in: Option<u32>,
    preview_in: Option<u32>,
    /// Last keyframe request to this member per layer (high, low): coalesces requests of
    /// many viewers.
    last_pli: [Option<Instant>; 2],
    share: Option<ShareInfo>,
    /// Whose share this member watches.
    watch: Option<Watch>,
    /// Whose previews this member shows.
    previews: HashSet<PeerId>,
    /// Streams declared on this session (audio, share audio, previews): source SSRC →
    /// offset subtracted from the source's extended sequence numbers, so each stream
    /// starts with rollover counter 0 (the receiver's SRTP assumption) however long the
    /// source has been sending. The main video stream is kept in [`Watch`].
    out: HashMap<u32, u64>,
}

impl Member {
    fn ssrcs(&self) -> [u32; 6] {
        [
            self.ssrc,
            self.video_ssrc,
            self.video_rtx,
            self.screen_audio_ssrc,
            self.preview_ssrc,
            self.preview_rtx,
        ]
    }

    /// The share carries a preview layer this SFU can forward.
    fn has_low(&self) -> bool {
        self.share.is_some_and(|s| s.preview) && self.offers_preview
    }

    /// The share as told to others (the preview flag only if it can be forwarded).
    fn share_info(&self) -> Option<ShareInfo> {
        let has_low = self.has_low();
        self.share.map(|s| ShareInfo {
            preview: has_low,
            ..s
        })
    }
}

struct Forward {
    from: PeerId,
    room: String,
    kind: TrackKind,
    pt: Pt,
    marker: bool,
    /// Extended sequence number from the source's session.
    seq: u64,
    timestamp: u32,
    payload: Vec<u8>,
}

/// What draining members' Rtcs produced: RTP to forward, keyframe requests (viewer,
/// forwarded SSRC) and loss reports (viewer, forwarded SSRC, loss) of forwarded video.
#[derive(Default)]
struct Collected {
    forwards: Vec<Forward>,
    keyframes: Vec<(PeerId, u32)>,
    losses: Vec<(PeerId, u32, f32)>,
}

impl Collected {
    fn is_empty(&self) -> bool {
        self.forwards.is_empty() && self.keyframes.is_empty() && self.losses.is_empty()
    }

    fn extend(&mut self, other: Collected) {
        self.forwards.extend(other.forwards);
        self.keyframes.extend(other.keyframes);
        self.losses.extend(other.losses);
    }
}

struct Worker {
    socket: Option<UdpSocket>,
    udp_port: u16,
    high_budget: Option<u32>,
    cmds: Receiver<Cmd>,
    peers: HashMap<PeerId, Member>,
    next_ssrc: u32,
    next_order: u64,
    buf: Vec<u8>,
    /// RTP received since the last forward (collected right after each input).
    received: Collected,
    st: SfuStats,
    stats: Arc<Mutex<SfuStats>>,
}

/// Budget charged for a high stream whose share did not announce a bitrate.
const UNKNOWN_SHARE_KBPS: u32 = 8000;
/// At most one keyframe request per layer of a sharer per this interval.
const PLI_INTERVAL: Duration = Duration::from_millis(100);

fn layer_index(layer: Layer) -> usize {
    match layer {
        Layer::High => 0,
        Layer::Low => 1,
    }
}

impl Worker {
    fn run(mut self) {
        let mut last_publish = Instant::now();
        loop {
            // Idle: block on commands instead of polling.
            if self.peers.is_empty() {
                match self.cmds.recv_timeout(Duration::from_millis(200)) {
                    Ok(cmd) => {
                        if !self.command(cmd) {
                            return;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            let mut busy = false;
            loop {
                match self.cmds.try_recv() {
                    Ok(cmd) => {
                        busy = true;
                        if !self.command(cmd) {
                            return;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
            busy |= self.receive_udp();
            let now = Instant::now();
            for m in self.peers.values_mut() {
                if now >= m.next_timeout {
                    let _ = m.rtc.handle_input(Input::Timeout(now));
                }
            }
            let mut got = std::mem::take(&mut self.received);
            got.extend(self.drain());
            if !got.is_empty() {
                busy = true;
                self.handle(got);
                // Send what forwarding queued right away.
                let more = self.drain();
                self.handle(more);
            }
            let dead: Vec<PeerId> = self
                .peers
                .iter()
                .filter(|(_, m)| !m.rtc.is_alive())
                .map(|(id, _)| id.clone())
                .collect();
            for id in dead {
                self.remove(&id);
            }
            if last_publish.elapsed() >= Duration::from_millis(500) {
                self.publish();
                last_publish = Instant::now();
            }
            if !busy {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    fn handle(&mut self, got: Collected) {
        for (viewer, ssrc) in got.keyframes {
            self.viewer_keyframe(&viewer, ssrc);
        }
        let mut changed = false;
        for (viewer, ssrc, loss) in got.losses {
            changed |= self.loss_report(&viewer, ssrc, loss);
        }
        if changed {
            self.decide();
        }
        self.forward(got.forwards);
    }

    /// Returns `false` on stop.
    fn command(&mut self, cmd: Cmd) -> bool {
        match cmd {
            Cmd::Stop => return false,
            Cmd::Join {
                peer,
                room,
                offer,
                link,
                mut sink,
            } => {
                self.remove(&peer);
                match self.accept(&room, &offer, &link) {
                    Ok((rtc, answer, local)) => {
                        sink.answer(Ok(answer));
                        let mut ssrcs = [0u32; 6];
                        for i in 0..ssrcs.len() {
                            ssrcs[i] = self.new_ssrc_besides(&ssrcs[..i]);
                        }
                        let [
                            ssrc,
                            video_ssrc,
                            video_rtx,
                            screen_audio_ssrc,
                            preview_ssrc,
                            preview_rtx,
                        ] = ssrcs;
                        let offers_preview = count_video_mlines(&offer) >= 2;
                        let mine = [
                            audio_track(ssrc),
                            screen_track(video_ssrc, video_rtx),
                            screen_audio_track(screen_audio_ssrc),
                        ];
                        let my_preview = preview_track(preview_ssrc, preview_rtx);
                        for (id, m) in self.peers.iter_mut().filter(|(_, m)| m.room == room) {
                            sink.track(id, audio_track(m.ssrc), true);
                            sink.track(id, screen_track(m.video_ssrc, m.video_rtx), true);
                            sink.track(id, screen_audio_track(m.screen_audio_ssrc), true);
                            // Previews only to sessions that can carry them (an S3 client
                            // would take an unknown kind for audio).
                            if offers_preview {
                                sink.track(id, preview_track(m.preview_ssrc, m.preview_rtx), true);
                            }
                            if m.share.is_some() {
                                sink.screen(id, m.share_info());
                            }
                            for t in mine {
                                m.sink.track(&peer, t, true);
                            }
                            if m.offers_preview {
                                m.sink.track(&peer, my_preview, true);
                            }
                        }
                        self.peers.insert(
                            peer,
                            Member {
                                room,
                                rtc,
                                sink,
                                local,
                                tunnel: link == Link::Tunnel,
                                mid: None,
                                pt: None,
                                video_mid: None,
                                preview_mid: None,
                                offers_preview,
                                video_pts: Vec::new(),
                                connected: false,
                                next_timeout: Instant::now(),
                                ssrc,
                                video_ssrc,
                                video_rtx,
                                screen_audio_mid: None,
                                screen_audio_ssrc,
                                preview_ssrc,
                                preview_rtx,
                                video_in: None,
                                preview_in: None,
                                last_pli: [None, None],
                                share: None,
                                watch: None,
                                previews: HashSet::new(),
                                out: HashMap::new(),
                            },
                        );
                    }
                    Err(e) => sink.answer(Err(e)),
                }
            }
            Cmd::Leave(peer) => self.remove(&peer),
            Cmd::Share(peer, share) => self.set_share(&peer, share),
            Cmd::Subscribe(peer, source, layer) => self.set_watching(&peer, source, layer),
            Cmd::Previews(peer, sources) => self.set_previews(&peer, sources),
            Cmd::Datagram(peer, data) => {
                self.st.tunnel_in += 1;
                if let Some(m) = self.peers.get_mut(&peer).filter(|m| m.tunnel)
                    && let Ok(contents) = data.as_slice().try_into()
                {
                    let input = Input::Receive(
                        Instant::now(),
                        Receive {
                            proto: Protocol::Udp,
                            source: TUNNEL_CLIENT,
                            destination: TUNNEL_SERVER,
                            contents,
                        },
                    );
                    if m.rtc.accepts(&input) {
                        let _ = m.rtc.handle_input(input);
                        // RTP mode keeps one received packet: take it before the next.
                        drain_member(
                            &peer,
                            m,
                            self.socket.as_ref(),
                            &mut self.st,
                            &mut self.received,
                        );
                    }
                }
            }
        }
        true
    }

    /// Drops a member's session and its stream in every other session of the room. A new
    /// session of the same member gets a new SSRC (and so a new sequence space).
    fn remove(&mut self, peer: &str) {
        let Some(mut gone) = self.peers.remove(peer) else {
            return;
        };
        gone.rtc.disconnect();
        for m in self.peers.values_mut().filter(|m| m.room == gone.room) {
            for ssrc in [
                gone.ssrc,
                gone.video_ssrc,
                gone.screen_audio_ssrc,
                gone.preview_ssrc,
            ] {
                if m.out.remove(&ssrc).is_some() || ssrc == gone.video_ssrc {
                    m.rtc.direct_api().remove_stream_tx(Ssrc::from(ssrc));
                }
            }
            m.sink.track(peer, audio_track(gone.ssrc), false);
            m.sink
                .track(peer, screen_track(gone.video_ssrc, gone.video_rtx), false);
            m.sink
                .track(peer, screen_audio_track(gone.screen_audio_ssrc), false);
            if m.offers_preview {
                m.sink.track(
                    peer,
                    preview_track(gone.preview_ssrc, gone.preview_rtx),
                    false,
                );
            }
            if gone.share.is_some() {
                m.sink.screen(peer, None);
            }
            if m.watch.as_ref().is_some_and(|w| w.source == peer) {
                m.watch = None;
            }
            m.previews.remove(peer);
        }
        self.decide();
    }

    fn set_share(&mut self, peer: &str, share: Option<ShareInfo>) {
        let Some(m) = self.peers.get_mut(peer) else {
            return;
        };
        if m.share == share {
            return;
        }
        let was_sharing = m.share.is_some();
        m.share = share;
        let info = m.share_info();
        let room = m.room.clone();
        for (_, other) in self
            .peers
            .iter_mut()
            .filter(|(id, o)| id.as_str() != peer && o.room == room)
        {
            other.sink.screen(peer, info);
        }
        self.decide();
        if share.is_some() && !was_sharing {
            // Viewers that were already waiting need a decodable start. (The client's
            // sequence numbers continue across shares on one session, so a running main
            // stream simply goes on.)
            self.request_keyframe(peer, Layer::High);
            self.request_keyframe(peer, Layer::Low);
        }
    }

    fn set_watching(&mut self, peer: &str, source: Option<PeerId>, requested: LayerChoice) {
        let Some(m) = self.peers.get_mut(peer) else {
            return;
        };
        let current = m.watch.as_ref().map(|w| w.source.clone());
        if current.is_some() && current == source {
            let w = m.watch.as_mut().expect("watching");
            if w.requested != requested {
                w.requested = requested;
                w.told = None;
                self.decide();
            } else {
                // A repeated subscribe is how a viewer asks to start over (e.g. after a
                // decoder error): give it a keyframe and tell it the layer again.
                w.told = None;
                let layer = w.pending.or(w.layer).unwrap_or(w.target);
                let s = w.source.clone();
                self.request_keyframe(&s, layer);
                self.decide();
            }
            return;
        }
        // A fresh stream for every new subscription: the viewer restarts its receive
        // stream.
        if let Some(old) = m.watch.take()
            && let Some(o) = self.peers.get(&old.source)
        {
            let ssrc = o.video_ssrc;
            let audio = o.screen_audio_ssrc;
            if let Some(m) = self.peers.get_mut(peer) {
                m.rtc.direct_api().remove_stream_tx(Ssrc::from(ssrc));
                if m.out.remove(&audio).is_some() {
                    m.rtc.direct_api().remove_stream_tx(Ssrc::from(audio));
                }
            }
        }
        if let Some(source) = source {
            let same_room = self
                .peers
                .get(&source)
                .zip(self.peers.get(peer))
                .is_some_and(|(s, m)| s.room == m.room && source != peer);
            if same_room {
                self.next_order += 1;
                let order = self.next_order;
                if let Some(m) = self.peers.get_mut(peer) {
                    m.watch = Some(Watch::new(source, requested, order));
                }
            }
        }
        self.decide();
    }

    fn set_previews(&mut self, peer: &str, sources: Vec<PeerId>) {
        let Some(m) = self.peers.get_mut(peer) else {
            return;
        };
        let new: HashSet<PeerId> = sources.into_iter().filter(|s| s != peer).collect();
        let dropped: Vec<PeerId> = m.previews.difference(&new).cloned().collect();
        let added: Vec<PeerId> = new.difference(&m.previews).cloned().collect();
        m.previews = new;
        for d in dropped {
            let Some(ssrc) = self.peers.get(&d).map(|o| o.preview_ssrc) else {
                continue;
            };
            if let Some(m) = self.peers.get_mut(peer)
                && m.out.remove(&ssrc).is_some()
            {
                m.rtc.direct_api().remove_stream_tx(Ssrc::from(ssrc));
            }
        }
        for a in added {
            self.request_keyframe(&a, Layer::Low);
        }
    }

    /// Re-decides every viewer's layer: the request, its network and the high budget
    /// (earlier subscriptions first). Tells viewers whose layer or reason changed and asks
    /// sharers for keyframes of layers that start.
    fn decide(&mut self) {
        let mut viewers: Vec<(u64, PeerId)> = self
            .peers
            .iter()
            .filter_map(|(id, m)| m.watch.as_ref().map(|w| (w.order, id.clone())))
            .collect();
        viewers.sort();
        let mut used: u64 = 0;
        let mut keyframes: Vec<(PeerId, Layer)> = Vec::new();
        for (_, id) in viewers {
            let Some(w) = self.peers.get(&id).and_then(|m| m.watch.as_ref()) else {
                continue;
            };
            let Some(src) = self.peers.get(&w.source) else {
                continue;
            };
            let Some(share) = src.share else {
                // Not sharing (yet): nothing flows, the decision waits.
                continue;
            };
            let cost = u64::from(if share.bitrate_kbps > 0 {
                share.bitrate_kbps
            } else {
                UNKNOWN_SHARE_KBPS
            });
            let has_low = src.has_low();
            let admitted = self.high_budget.is_none_or(|b| used + cost <= u64::from(b));
            let (target, reason) =
                layer::choose(w.requested, has_low, w.net.allows_high(), admitted);
            if target == Layer::High && has_low {
                used += cost;
            }
            let source = w.source.clone();
            let Some(m) = self.peers.get_mut(&id) else {
                continue;
            };
            let w = m.watch.as_mut().expect("watching");
            w.target = target;
            w.reason = reason;
            match w.layer {
                // Nothing went out yet: start with the target layer directly.
                None => {
                    if w.pending != Some(target) {
                        w.pending = Some(target);
                        keyframes.push((source.clone(), target));
                    }
                }
                Some(l) if l == target => w.pending = None,
                Some(_) => {
                    if w.pending != Some(target) {
                        w.pending = Some(target);
                        keyframes.push((source.clone(), target));
                    }
                }
            }
            if w.told != Some((target, reason)) {
                w.told = Some((target, reason));
                let sub = Subscription {
                    source,
                    requested: w.requested,
                    layer: target,
                    reason,
                };
                m.sink.subscription(&sub);
            }
        }
        self.st.high_egress_kbps = used;
        for (source, layer) in keyframes {
            self.request_keyframe(&source, layer);
        }
    }

    /// A viewer's receiver report on a forwarded video stream (main or preview).
    fn loss_report(&mut self, viewer: &str, ssrc: u32, loss: f32) -> bool {
        let source_main = self
            .peers
            .iter()
            .find(|(_, m)| m.video_ssrc == ssrc)
            .map(|(id, _)| id.clone());
        let Some(source) = source_main else {
            return false;
        };
        let Some(w) = self
            .peers
            .get_mut(viewer)
            .and_then(|m| m.watch.as_mut())
            .filter(|w| w.source == source)
        else {
            return false;
        };
        w.net.report(Instant::now(), loss)
    }

    /// Asks a sharer's client for a keyframe of one layer (PLI on its incoming stream).
    fn request_keyframe(&mut self, source: &str, layer: Layer) {
        let Some(m) = self.peers.get_mut(source) else {
            return;
        };
        let incoming = match layer {
            Layer::High => m.video_in,
            Layer::Low => m.preview_in,
        };
        let i = layer_index(layer);
        if m.last_pli[i].is_some_and(|t| t.elapsed() < PLI_INTERVAL) {
            return;
        }
        if let Some(ssrc) = incoming
            && let Some(rx) = m.rtc.direct_api().stream_rx(&Ssrc::from(ssrc))
        {
            rx.request_keyframe(KeyframeRequestKind::Pli);
            m.last_pli[i] = Some(Instant::now());
            self.st.keyframe_requests += 1;
        }
    }

    /// A viewer asked for a keyframe of a forwarded stream (main or preview).
    fn viewer_keyframe(&mut self, viewer: &str, ssrc: u32) {
        let found = self.peers.iter().find_map(|(id, m)| {
            if m.video_ssrc == ssrc {
                Some((id.clone(), true))
            } else if m.preview_ssrc == ssrc {
                Some((id.clone(), false))
            } else {
                None
            }
        });
        let Some((source, main)) = found else {
            return;
        };
        let layer = if main {
            self.peers
                .get(viewer)
                .and_then(|m| m.watch.as_ref())
                .filter(|w| w.source == source)
                .map_or(Layer::High, |w| w.pending.or(w.layer).unwrap_or(w.target))
        } else {
            Layer::Low
        };
        self.request_keyframe(&source, layer);
    }

    /// A fresh SSRC, unique among live sessions and not in `taken`.
    fn new_ssrc_besides(&mut self, taken: &[u32]) -> u32 {
        loop {
            // Weyl sequence: distinct values for 2^32 calls.
            self.next_ssrc = self.next_ssrc.wrapping_add(0x9E37_79B9);
            let s = self.next_ssrc;
            let used = taken.contains(&s) || self.peers.values().any(|m| m.ssrcs().contains(&s));
            if s != 0 && !used {
                return s;
            }
        }
    }

    fn accept(
        &self,
        _room: &str,
        offer: &str,
        link: &Link,
    ) -> Result<(Rtc, String, SocketAddr), String> {
        // Standard extensions minus `mid` (see the crate docs).
        let mut exts = ExtensionMap::empty();
        exts.set(1, Extension::AudioLevel);
        exts.set(2, Extension::AbsoluteSendTime);
        exts.set(3, Extension::TransportSequenceNumber);
        let mut rtc = Rtc::builder()
            .set_ice_lite(true)
            .set_rtp_mode(true)
            .clear_codecs()
            .enable_opus(true, false)
            .enable_h264(true)
            .set_extension_map(exts)
            // Receiver reports of forwarded video drive the per-viewer layer choice.
            .set_stats_interval(Some(Duration::from_secs(1)))
            .build(Instant::now());
        let local = match link {
            Link::Tunnel => {
                let c = Candidate::host(TUNNEL_SERVER, "udp").map_err(|e| e.to_string())?;
                rtc.add_local_candidate(c);
                TUNNEL_SERVER
            }
            Link::Udp { candidate_ips } => {
                if self.socket.is_none() {
                    return Err("UDP media is disabled on this server".into());
                }
                let mut first = None;
                let mut seen = Vec::new();
                for ip in candidate_ips {
                    if seen.contains(ip) {
                        continue;
                    }
                    seen.push(*ip);
                    let addr = SocketAddr::new(*ip, self.udp_port);
                    if let Ok(c) = Candidate::host(addr, "udp") {
                        rtc.add_local_candidate(c);
                        first.get_or_insert(addr);
                    }
                }
                first.ok_or("no usable UDP candidate address")?
            }
        };
        let offer = SdpOffer::from_sdp_string(offer).map_err(|e| format!("bad offer: {e}"))?;
        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|e| e.to_string())?;
        Ok((rtc, answer.to_sdp_string(), local))
    }

    /// Feeds every queued UDP datagram to the member it belongs to.
    fn receive_udp(&mut self) -> bool {
        let Some(socket) = &self.socket else {
            return false;
        };
        let mut any = false;
        loop {
            let (n, source) = match socket.recv_from(&mut self.buf) {
                Ok(r) => r,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                // ICMP port unreachable from an earlier send (Windows) and similar.
                Err(_) => continue,
            };
            any = true;
            self.st.udp_in += 1;
            for (id, m) in self.peers.iter_mut().filter(|(_, m)| !m.tunnel) {
                let Ok(contents) = self.buf[..n].try_into() else {
                    break;
                };
                // The destination we stamp is the member's first candidate: with one socket
                // on 0.0.0.0 the real one is unknown (and behind a port forward it differs
                // from the public address the member dialed anyway).
                let input = Input::Receive(
                    Instant::now(),
                    Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: m.local,
                        contents,
                    },
                );
                if m.rtc.accepts(&input) {
                    let _ = m.rtc.handle_input(input);
                    // RTP mode keeps one received packet: a second datagram handed in
                    // before polling would overwrite it (lost whenever a member's packets
                    // arrive in a burst).
                    drain_member(id, m, Some(socket), &mut self.st, &mut self.received);
                    break;
                }
            }
        }
        any
    }

    /// Sends everything the members' Rtcs want to send and collects received RTP.
    fn drain(&mut self) -> Collected {
        let mut got = Collected::default();
        for (id, m) in self.peers.iter_mut() {
            drain_member(id, m, self.socket.as_ref(), &mut self.st, &mut got);
        }
        got
    }

    fn forward(&mut self, forwards: Vec<Forward>) {
        for f in forwards {
            let (out_ssrc, out_rtx, preview_ssrc, preview_rtx, source_has_low) =
                match self.peers.get(&f.from) {
                    Some(s) => (
                        match f.kind {
                            TrackKind::Audio => s.ssrc,
                            TrackKind::ScreenAudio => s.screen_audio_ssrc,
                            TrackKind::Screen | TrackKind::ScreenPreview => s.video_ssrc,
                        },
                        s.video_rtx,
                        s.preview_ssrc,
                        s.preview_rtx,
                        s.has_low(),
                    ),
                    None => continue,
                };
            let layer = if f.kind == TrackKind::ScreenPreview {
                Layer::Low
            } else {
                Layer::High
            };
            let keyframe_start = matches!(f.kind, TrackKind::Screen | TrackKind::ScreenPreview)
                && starts_keyframe(&f.payload);
            for (id, m) in self.peers.iter_mut() {
                if *id == f.from || m.room != f.room || !m.connected {
                    continue;
                }
                match f.kind {
                    TrackKind::Audio => {
                        if let (Some(mid), Some(pt)) = (m.mid, m.pt) {
                            write_stream(m, &mut self.st, mid, pt, out_ssrc, None, &f, false);
                        }
                    }
                    TrackKind::ScreenAudio => {
                        if m.watch.as_ref().is_some_and(|w| w.source == f.from)
                            && let (Some(mid), Some(pt)) = (m.screen_audio_mid, m.pt)
                        {
                            write_stream(m, &mut self.st, mid, pt, out_ssrc, None, &f, false);
                        }
                    }
                    TrackKind::Screen | TrackKind::ScreenPreview => {
                        // Both ends run the same codec table; fall back to the first H.264
                        // type if this session negotiated a different number.
                        let Some(pt) = (if m.video_pts.contains(&f.pt) {
                            Some(f.pt)
                        } else {
                            m.video_pts.first().copied()
                        }) else {
                            continue;
                        };
                        if layer == Layer::Low
                            && m.previews.contains(&f.from)
                            && let Some(mid) = m.preview_mid
                            && write_stream(
                                m,
                                &mut self.st,
                                mid,
                                pt,
                                preview_ssrc,
                                Some(preview_rtx),
                                &f,
                                true,
                            )
                        {
                            self.st.preview_out += 1;
                        }
                        if let Some(mid) = m.video_mid
                            && m.watch.as_ref().is_some_and(|w| w.source == f.from)
                        {
                            write_main(
                                m,
                                &mut self.st,
                                mid,
                                pt,
                                (out_ssrc, out_rtx),
                                layer,
                                source_has_low,
                                keyframe_start,
                                &f,
                            );
                        }
                    }
                }
            }
        }
    }

    fn publish(&mut self) {
        self.st.peers = self.peers.len();
        self.st.tunnel_peers = self.peers.values().filter(|m| m.tunnel).count();
        self.st.connected_peers = self.peers.values().filter(|m| m.connected).count();
        self.st.streams_out = self
            .peers
            .values()
            .map(|m| m.out.len() + usize::from(m.watch.as_ref().is_some_and(|w| w.layer.is_some())))
            .sum();
        let targets = self.peers.values().filter_map(|m| {
            let w = m.watch.as_ref()?;
            self.peers.get(&w.source)?.share?;
            Some(w.pending.or(w.layer).unwrap_or(w.target))
        });
        let (mut high, mut low) = (0, 0);
        for t in targets {
            match t {
                Layer::High => high += 1,
                Layer::Low => low += 1,
            }
        }
        self.st.viewers_high = high;
        self.st.viewers_low = low;
        self.st.preview_subscriptions = self
            .peers
            .values()
            .map(|m| {
                m.previews
                    .iter()
                    .filter(|p| self.peers.get(*p).is_some_and(Member::has_low))
                    .count()
            })
            .sum();
        if let Ok(mut s) = self.stats.lock() {
            let dropped = s.dropped_commands;
            *s = self.st.clone();
            s.dropped_commands = dropped;
        }
    }
}

/// Writes a packet into one of the member's base-mapped streams (declared on first use).
/// Returns whether it was written.
#[allow(clippy::too_many_arguments)]
fn write_stream(
    m: &mut Member,
    st: &mut SfuStats,
    mid: Mid,
    pt: Pt,
    ssrc: u32,
    rtx: Option<u32>,
    f: &Forward,
    video: bool,
) -> bool {
    let base = match m.out.entry(ssrc) {
        Entry::Occupied(e) => *e.get(),
        Entry::Vacant(e) => {
            declare(&mut m.rtc, mid, ssrc, rtx);
            *e.insert(f.seq & !0xFFFF)
        }
    };
    // A packet reordered across the first one's rollover would need a negative index:
    // drop it (it is late anyway).
    let Some(seq) = f.seq.checked_sub(base) else {
        return false;
    };
    write(m, st, pt, ssrc, seq, f, video)
}

/// Writes a packet of one of the source's layers into the member's main video stream:
/// the forwarded layer as is (sequence numbers shifted), a pending layer from its next
/// keyframe on.
#[allow(clippy::too_many_arguments)]
fn write_main(
    m: &mut Member,
    st: &mut SfuStats,
    mid: Mid,
    pt: Pt,
    (ssrc, rtx): (u32, u32),
    layer: Layer,
    source_has_low: bool,
    keyframe_start: bool,
    f: &Forward,
) {
    let Some(w) = m.watch.as_mut() else {
        return;
    };
    if !source_has_low && layer == Layer::Low {
        return;
    }
    let switching = w.pending == Some(layer) && w.layer != Some(layer) && keyframe_start;
    if switching {
        let first = w.layer.is_none();
        if first {
            declare(&mut m.rtc, mid, ssrc, Some(rtx));
        } else {
            st.layer_switches += 1;
        }
        // Continue the outgoing sequence (a fresh stream starts at a small number so the
        // receiver's SRTP rollover counter starts at 0).
        let next = w.last_out.map_or(f.seq & 0xFFFF, |l| l + 1);
        w.offset = next as i64 - f.seq as i64;
        w.layer = Some(layer);
        w.pending = None;
    }
    if w.layer != Some(layer) {
        return;
    }
    let out = f.seq as i64 + w.offset;
    if out < 0 {
        return;
    }
    let out = out as u64;
    w.last_out = Some(w.last_out.map_or(out, |l| l.max(out)));
    write(m, st, pt, ssrc, out, f, true);
}

fn declare(rtc: &mut Rtc, mid: Mid, ssrc: u32, rtx: Option<u32>) {
    // str0m's pacer queues by (mid, rid), so every stream on one m-line needs its own rid.
    // The rid header extension is not negotiated: the rid never goes on the wire.
    let rid = Rid::from(format!("{ssrc:08x}").as_str());
    rtc.direct_api()
        .declare_stream_tx(Ssrc::from(ssrc), rtx.map(Ssrc::from), mid, Some(rid));
}

fn write(
    m: &mut Member,
    st: &mut SfuStats,
    pt: Pt,
    ssrc: u32,
    seq: u64,
    f: &Forward,
    video: bool,
) -> bool {
    let write = RtpWrite::new(
        pt,
        SeqNo::from(seq),
        f.timestamp,
        Instant::now(),
        f.payload.clone(),
    )
    .marker(f.marker)
    .nackable(video);
    match m.rtc.direct_api().stream_tx(&Ssrc::from(ssrc)) {
        Some(tx) => {
            tx.write_rtp(write);
            st.rtp_out += 1;
            if video {
                st.video_out += 1;
            }
            true
        }
        None => false,
    }
}

fn audio_track(ssrc: u32) -> Track {
    Track {
        kind: TrackKind::Audio,
        ssrc,
        rtx: None,
    }
}

fn screen_audio_track(ssrc: u32) -> Track {
    Track {
        kind: TrackKind::ScreenAudio,
        ssrc,
        rtx: None,
    }
}

fn screen_track(ssrc: u32, rtx: u32) -> Track {
    Track {
        kind: TrackKind::Screen,
        ssrc,
        rtx: Some(rtx),
    }
}

fn preview_track(ssrc: u32, rtx: u32) -> Track {
    Track {
        kind: TrackKind::ScreenPreview,
        ssrc,
        rtx: Some(rtx),
    }
}

/// Number of video m-lines in an SDP offer.
fn count_video_mlines(sdp: &str) -> usize {
    sdp.lines().filter(|l| l.starts_with("m=video")).count()
}

/// Sends what one member's Rtc wants to send and collects the RTP it received.
fn drain_member(
    id: &PeerId,
    m: &mut Member,
    socket: Option<&UdpSocket>,
    st: &mut SfuStats,
    got: &mut Collected,
) {
    loop {
        match m.rtc.poll_output() {
            Ok(Output::Timeout(t)) => {
                m.next_timeout = t;
                break;
            }
            Ok(Output::Transmit(t)) => {
                if m.tunnel {
                    st.tunnel_out += 1;
                    m.sink.datagram(t.contents.to_vec());
                } else if let Some(s) = socket {
                    st.udp_out += 1;
                    if s.send_to(&t.contents, t.destination).is_err() {
                        st.udp_send_errors += 1;
                    }
                }
            }
            Ok(Output::Event(e)) => match e {
                Event::Connected => {
                    m.connected = true;
                    m.pt = m
                        .rtc
                        .codec_config()
                        .find(|p| p.spec().codec == Codec::Opus)
                        .map(|p| p.pt());
                    m.video_pts = m
                        .rtc
                        .codec_config()
                        .iter()
                        .filter(|p| p.spec().codec == Codec::H264)
                        .map(|p| p.pt())
                        .collect();
                }
                // The first audio m-line is the voice, a second one the share's audio.
                Event::MediaAdded(a) if a.kind == MediaKind::Audio => {
                    if m.mid.is_none() || m.mid == Some(a.mid) {
                        m.mid = Some(a.mid);
                    } else {
                        m.screen_audio_mid = Some(a.mid);
                    }
                }
                // The first video m-line is the main share, a second one the previews.
                Event::MediaAdded(a) if a.kind == MediaKind::Video => {
                    if m.video_mid.is_none() || m.video_mid == Some(a.mid) {
                        m.video_mid = Some(a.mid);
                    } else {
                        m.preview_mid = Some(a.mid);
                    }
                }
                Event::RtpPacket(p) => {
                    st.rtp_in += 1;
                    let ssrc = *p.header.ssrc;
                    let mid = m
                        .rtc
                        .direct_api()
                        .stream_rx(&p.header.ssrc)
                        .map(|s| s.mid());
                    let video = mid.is_some() && mid == m.video_mid;
                    let preview = mid.is_some() && mid == m.preview_mid;
                    let screen_audio = mid.is_some() && mid == m.screen_audio_mid;
                    if video {
                        m.video_in = Some(ssrc);
                    }
                    if preview {
                        m.preview_in = Some(ssrc);
                        st.preview_in += 1;
                    }
                    if video || preview {
                        st.video_in += 1;
                    }
                    // Only a member that announced its share is forwarded.
                    if (video || preview || screen_audio) && m.share.is_none() {
                        continue;
                    }
                    let kind = if video {
                        TrackKind::Screen
                    } else if preview {
                        TrackKind::ScreenPreview
                    } else if screen_audio {
                        TrackKind::ScreenAudio
                    } else {
                        TrackKind::Audio
                    };
                    got.forwards.push(Forward {
                        from: id.clone(),
                        room: m.room.clone(),
                        kind,
                        pt: p.header.payload_type,
                        marker: p.header.marker,
                        seq: *p.seq_no,
                        timestamp: p.header.timestamp,
                        payload: p.payload.to_vec(),
                    });
                }
                Event::KeyframeRequest(r) => {
                    // Forwarded streams carry their SSRC (hex) as their rid.
                    if let Some(ssrc) = r.rid.and_then(|rid| rid_ssrc(&rid)) {
                        got.keyframes.push((id.clone(), ssrc));
                    }
                }
                Event::MediaEgressStats(s) if Some(s.mid) == m.video_mid => {
                    if let (Some(loss), Some(ssrc)) = (s.loss, s.rid.and_then(|r| rid_ssrc(&r))) {
                        got.losses.push((id.clone(), ssrc, loss));
                    }
                }
                _ => {}
            },
            Err(_) => {
                m.rtc.disconnect();
                break;
            }
        }
    }
}

fn rid_ssrc(rid: &Rid) -> Option<u32> {
    u32::from_str_radix(&rid.to_string(), 16).ok()
}

/// Start of the SSRC sequence: differs between server runs, so a client that missed a
/// restart does not mistake new streams for old ones.
fn first_ssrc() -> u32 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    t.subsec_nanos() ^ (t.as_secs() as u32).rotate_left(16)
}

#[cfg(test)]
mod tests {
    use super::count_video_mlines;

    #[test]
    fn counts_video_mlines() {
        let sdp = "v=0\r\nm=audio 9 UDP\r\nm=video 9 UDP\r\nm=audio 9\r\nm=video 9\r\n";
        assert_eq!(count_video_mlines(sdp), 2);
        assert_eq!(count_video_mlines("v=0\r\nm=audio 9\r\n"), 0);
    }
}
