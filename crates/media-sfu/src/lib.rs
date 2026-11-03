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

use std::collections::HashMap;
use std::collections::hash_map::Entry;
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
use turbo_pancake_media_transport::{TUNNEL_CLIENT, TUNNEL_SERVER, init_crypto};

pub type PeerId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Audio,
    /// Screen share video (H.264) on the video m-line.
    Screen,
    /// The share's system audio (Opus) on the screen audio m-line.
    ScreenAudio,
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
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SfuStats {
    pub peers: usize,
    pub tunnel_peers: usize,
    pub connected_peers: usize,
    pub rtp_in: u64,
    pub rtp_out: u64,
    /// Screen share RTP received / forwarded (included in `rtp_in` / `rtp_out`).
    pub video_in: u64,
    pub video_out: u64,
    /// Keyframe requests passed from viewers to sharers.
    pub keyframe_requests: u64,
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
    Subscribe(PeerId, Option<PeerId>),
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
            cmds: rx,
            peers: HashMap::new(),
            next_ssrc: first_ssrc(),
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

    /// The member watches `source`'s share (`None` = none). Replaces the previous
    /// subscription; the sharer is asked for a keyframe.
    pub fn subscribe(&self, peer: &str, source: Option<&str>) -> bool {
        self.send(Cmd::Subscribe(peer.to_owned(), source.map(str::to_owned)))
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
    /// The SSRC this member's client sends its share on (known after the first packet).
    video_in: Option<u32>,
    share: Option<ShareInfo>,
    /// Whose share this member watches.
    watching: Option<PeerId>,
    /// Streams declared on this session: source SSRC → offset subtracted from the source's
    /// extended sequence numbers, so each stream starts with rollover counter 0 (the
    /// receiver's SRTP assumption) however long the source has been sending.
    out: HashMap<u32, u64>,
}

struct Forward {
    from: PeerId,
    room: String,
    kind: TrackKind,
    ssrc: u32,
    rtx: Option<u32>,
    pt: Pt,
    marker: bool,
    /// Extended sequence number from the source's session.
    seq: u64,
    timestamp: u32,
    payload: Vec<u8>,
}

/// What draining members' Rtcs produced: RTP to forward and keyframe requests (forwarded
/// screen SSRC) to pass to the sharers.
#[derive(Default)]
struct Collected {
    forwards: Vec<Forward>,
    keyframes: Vec<u32>,
}

struct Worker {
    socket: Option<UdpSocket>,
    udp_port: u16,
    cmds: Receiver<Cmd>,
    peers: HashMap<PeerId, Member>,
    next_ssrc: u32,
    buf: Vec<u8>,
    /// RTP received since the last forward (collected right after each input).
    received: Collected,
    st: SfuStats,
    stats: Arc<Mutex<SfuStats>>,
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
            let more = self.drain();
            got.forwards.extend(more.forwards);
            got.keyframes.extend(more.keyframes);
            if !got.forwards.is_empty() || !got.keyframes.is_empty() {
                busy = true;
                self.keyframes(got.keyframes);
                self.forward(got.forwards);
                // Send what forwarding queued right away.
                let more = self.drain();
                self.keyframes(more.keyframes);
                self.forward(more.forwards);
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
                        let ssrc = self.new_ssrc();
                        let video_ssrc = self.new_ssrc_besides(&[ssrc]);
                        let video_rtx = self.new_ssrc_besides(&[ssrc, video_ssrc]);
                        let screen_audio_ssrc =
                            self.new_ssrc_besides(&[ssrc, video_ssrc, video_rtx]);
                        let mine = [
                            audio_track(ssrc),
                            screen_track(video_ssrc, video_rtx),
                            screen_audio_track(screen_audio_ssrc),
                        ];
                        for (id, m) in self.peers.iter_mut().filter(|(_, m)| m.room == room) {
                            sink.track(id, audio_track(m.ssrc), true);
                            sink.track(id, screen_track(m.video_ssrc, m.video_rtx), true);
                            sink.track(id, screen_audio_track(m.screen_audio_ssrc), true);
                            if m.share.is_some() {
                                sink.screen(id, m.share);
                            }
                            for t in mine {
                                m.sink.track(&peer, t, true);
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
                                video_pts: Vec::new(),
                                connected: false,
                                next_timeout: Instant::now(),
                                ssrc,
                                video_ssrc,
                                video_rtx,
                                screen_audio_mid: None,
                                screen_audio_ssrc,
                                video_in: None,
                                share: None,
                                watching: None,
                                out: HashMap::new(),
                            },
                        );
                    }
                    Err(e) => sink.answer(Err(e)),
                }
            }
            Cmd::Leave(peer) => self.remove(&peer),
            Cmd::Share(peer, share) => self.set_share(&peer, share),
            Cmd::Subscribe(peer, source) => self.set_watching(&peer, source),
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
            for ssrc in [gone.ssrc, gone.video_ssrc, gone.screen_audio_ssrc] {
                if m.out.remove(&ssrc).is_some() {
                    m.rtc.direct_api().remove_stream_tx(Ssrc::from(ssrc));
                }
            }
            m.sink.track(peer, audio_track(gone.ssrc), false);
            m.sink
                .track(peer, screen_track(gone.video_ssrc, gone.video_rtx), false);
            m.sink
                .track(peer, screen_audio_track(gone.screen_audio_ssrc), false);
            if gone.share.is_some() {
                m.sink.screen(peer, None);
            }
            if m.watching.as_deref() == Some(peer) {
                m.watching = None;
            }
        }
    }

    fn set_share(&mut self, peer: &str, share: Option<ShareInfo>) {
        let Some(m) = self.peers.get_mut(peer) else {
            return;
        };
        if m.share == share {
            return;
        }
        m.share = share;
        let room = m.room.clone();
        for (_, other) in self
            .peers
            .iter_mut()
            .filter(|(id, o)| id.as_str() != peer && o.room == room)
        {
            other.sink.screen(peer, share);
        }
        if share.is_some() {
            // Viewers that were already waiting need a decodable start.
            self.request_keyframe(peer);
        }
    }

    fn set_watching(&mut self, peer: &str, source: Option<PeerId>) {
        let Some(m) = self.peers.get_mut(peer) else {
            return;
        };
        if m.watching == source {
            // A repeated subscribe is how a viewer asks to start over (e.g. after a
            // decoder error): give it a keyframe.
            if let Some(s) = source {
                self.request_keyframe(&s);
            }
            return;
        }
        // A fresh stream for every subscription: the viewer restarts its receive stream.
        if let Some(old) = m.watching.take()
            && let Some(old_ssrcs) = self
                .peers
                .get(&old)
                .map(|o| [o.video_ssrc, o.screen_audio_ssrc])
            && let Some(m) = self.peers.get_mut(peer)
        {
            for old_ssrc in old_ssrcs {
                if m.out.remove(&old_ssrc).is_some() {
                    m.rtc.direct_api().remove_stream_tx(Ssrc::from(old_ssrc));
                }
            }
        }
        let Some(source) = source else {
            return;
        };
        let same_room = self
            .peers
            .get(&source)
            .zip(self.peers.get(peer))
            .is_some_and(|(s, m)| s.room == m.room);
        if same_room && let Some(m) = self.peers.get_mut(peer) {
            m.watching = Some(source.clone());
            self.request_keyframe(&source);
        }
    }

    /// Asks a sharer's client for a keyframe (PLI on its incoming video stream).
    fn request_keyframe(&mut self, source: &str) {
        let Some(m) = self.peers.get_mut(source) else {
            return;
        };
        if let Some(ssrc) = m.video_in
            && let Some(rx) = m.rtc.direct_api().stream_rx(&Ssrc::from(ssrc))
        {
            rx.request_keyframe(KeyframeRequestKind::Pli);
            self.st.keyframe_requests += 1;
        }
    }

    /// Keyframe requests from viewers, by forwarded screen SSRC.
    fn keyframes(&mut self, ssrcs: Vec<u32>) {
        for ssrc in ssrcs {
            let source = self
                .peers
                .iter()
                .find(|(_, m)| m.video_ssrc == ssrc)
                .map(|(id, _)| id.clone());
            if let Some(source) = source {
                self.request_keyframe(&source);
            }
        }
    }

    /// A fresh SSRC, unique among live sessions.
    fn new_ssrc(&mut self) -> u32 {
        self.new_ssrc_besides(&[])
    }

    /// A fresh SSRC, unique among live sessions and not in `taken`.
    fn new_ssrc_besides(&mut self, taken: &[u32]) -> u32 {
        loop {
            // Weyl sequence: distinct values for 2^32 calls.
            self.next_ssrc = self.next_ssrc.wrapping_add(0x9E37_79B9);
            let s = self.next_ssrc;
            let used = taken.contains(&s)
                || self
                    .peers
                    .values()
                    .any(|m| [m.ssrc, m.video_ssrc, m.video_rtx, m.screen_audio_ssrc].contains(&s));
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
            for (id, m) in self.peers.iter_mut() {
                if *id == f.from || m.room != f.room || !m.connected {
                    continue;
                }
                let (mid, pt) = match f.kind {
                    TrackKind::Audio => match (m.mid, m.pt) {
                        (Some(mid), Some(pt)) => (mid, pt),
                        _ => continue,
                    },
                    TrackKind::ScreenAudio => {
                        if m.watching.as_deref() != Some(f.from.as_str()) {
                            continue;
                        }
                        match (m.screen_audio_mid, m.pt) {
                            (Some(mid), Some(pt)) => (mid, pt),
                            _ => continue,
                        }
                    }
                    TrackKind::Screen => {
                        if m.watching.as_deref() != Some(f.from.as_str()) {
                            continue;
                        }
                        // Both ends run the same codec table; fall back to the first H.264
                        // type if this session negotiated a different number.
                        let pt = if m.video_pts.contains(&f.pt) {
                            Some(f.pt)
                        } else {
                            m.video_pts.first().copied()
                        };
                        match (m.video_mid, pt) {
                            (Some(mid), Some(pt)) => (mid, pt),
                            _ => continue,
                        }
                    }
                };
                let base = match m.out.entry(f.ssrc) {
                    Entry::Occupied(e) => *e.get(),
                    Entry::Vacant(e) => {
                        // str0m's pacer queues by (mid, rid), so every stream on the one
                        // m-line needs its own rid. The rid header extension is not
                        // negotiated: the rid never goes on the wire.
                        let rid = Rid::from(format!("{:08x}", f.ssrc).as_str());
                        m.rtc.direct_api().declare_stream_tx(
                            Ssrc::from(f.ssrc),
                            f.rtx.map(Ssrc::from),
                            mid,
                            Some(rid),
                        );
                        *e.insert(f.seq & !0xFFFF)
                    }
                };
                // A packet reordered across the first one's rollover would need a negative
                // index: drop it (it is late anyway).
                let Some(seq) = f.seq.checked_sub(base) else {
                    continue;
                };
                let write = RtpWrite::new(
                    pt,
                    SeqNo::from(seq),
                    f.timestamp,
                    Instant::now(),
                    f.payload.clone(),
                )
                .marker(f.marker)
                .nackable(f.kind == TrackKind::Screen);
                if let Some(tx) = m.rtc.direct_api().stream_tx(&Ssrc::from(f.ssrc)) {
                    tx.write_rtp(write);
                    self.st.rtp_out += 1;
                    if f.kind == TrackKind::Screen {
                        self.st.video_out += 1;
                    }
                }
            }
        }
    }

    fn publish(&mut self) {
        self.st.peers = self.peers.len();
        self.st.tunnel_peers = self.peers.values().filter(|m| m.tunnel).count();
        self.st.connected_peers = self.peers.values().filter(|m| m.connected).count();
        self.st.streams_out = self.peers.values().map(|m| m.out.len()).sum();
        if let Ok(mut s) = self.stats.lock() {
            let dropped = s.dropped_commands;
            *s = self.st.clone();
            s.dropped_commands = dropped;
        }
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
                Event::MediaAdded(a) if a.kind == MediaKind::Video => m.video_mid = Some(a.mid),
                Event::RtpPacket(p) => {
                    st.rtp_in += 1;
                    let ssrc = *p.header.ssrc;
                    let mid = m
                        .rtc
                        .direct_api()
                        .stream_rx(&p.header.ssrc)
                        .map(|s| s.mid());
                    let video = mid.is_some() && mid == m.video_mid;
                    let screen_audio = mid.is_some() && mid == m.screen_audio_mid;
                    if video {
                        st.video_in += 1;
                        m.video_in = Some(ssrc);
                    }
                    // Only a member that announced its share is forwarded.
                    if (video || screen_audio) && m.share.is_none() {
                        continue;
                    }
                    let (kind, out_ssrc) = if video {
                        (TrackKind::Screen, m.video_ssrc)
                    } else if screen_audio {
                        (TrackKind::ScreenAudio, m.screen_audio_ssrc)
                    } else {
                        (TrackKind::Audio, m.ssrc)
                    };
                    got.forwards.push(Forward {
                        from: id.clone(),
                        room: m.room.clone(),
                        kind,
                        ssrc: out_ssrc,
                        rtx: video.then_some(m.video_rtx),
                        pt: p.header.payload_type,
                        marker: p.header.marker,
                        seq: *p.seq_no,
                        timestamp: p.header.timestamp,
                        payload: p.payload.to_vec(),
                    });
                }
                Event::KeyframeRequest(r) => {
                    // Forwarded streams carry the source SSRC (hex) as their rid.
                    if let Some(ssrc) = r
                        .rid
                        .and_then(|rid| u32::from_str_radix(&rid.to_string(), 16).ok())
                    {
                        got.keyframes.push(ssrc);
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

/// Start of the SSRC sequence: differs between server runs, so a client that missed a
/// restart does not mistake new streams for old ones.
fn first_ssrc() -> u32 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    t.subsec_nanos() ^ (t.as_secs() as u32).rotate_left(16)
}
