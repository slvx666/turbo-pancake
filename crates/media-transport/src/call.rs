//! One 1:1 voice call: a [`UdpPeer`] driven on its own thread (spec §3: network work must
//! not block audio or UI).
//!
//! ```text
//! audio engine ─▶ bounded Receiver<AudioOut> ─▶ driver ─▶ RTP (Opus, PT from SDP) ─▶ UDP
//! UDP ─▶ driver ─▶ on_audio(AudioIn) callback (e.g. the engine's PacketSink, never blocks)
//! driver ─▶ bounded Receiver<CallEvent>: local SDP to relay, state changes, stats
//! signaling ─▶ CallDriver::remote_* commands (bounded, non-blocking)
//! ```
//!
//! The Rtc runs in RTP mode: received packets are handed over immediately with their
//! sequence numbers and our jitter buffer does reordering and concealment (str0m's frame
//! mode would hold up to 15 packets on a gap). Outgoing packets reuse the engine's
//! sequence numbers and RTP timestamps, so a frame index is identical on both sides.
//!
//! The offerer creates the SDP offer with one send/receive audio m-line (Opus only,
//! FEC via in-band LBRR, no RED); host candidates travel inside the SDP, extra ones
//! (later: server-reflexive/TURN) through [`CallDriver::remote_candidate`].
//!
//! With the SFU (ADR 0008) the server sends every other member as its own SSRC on the one
//! m-line; [`CallDriver::expect_source`] registers each SSRC the signaling announced, and
//! [`AudioIn::ssrc`] tells the streams apart.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::format::Codec;
use str0m::media::{Direction, MediaKind, Mid, Pt};
use str0m::rtp::{RtpWrite, SeqNo, Ssrc};
use str0m::{Candidate, Event, IceConnectionState, Rtc};

use crate::{TransportError, TunnelSend, UdpPeer};

/// How the call's datagrams travel.
pub enum MediaLink {
    /// A UDP socket bound to `CallConfig::bind`.
    Udp,
    /// Through the signaling connection (`MEDIA_TRANSPORT_WEBSOCKET`): datagrams from the
    /// server arrive on `inbound`, ours go to `outbound` (must not block).
    Tunnel {
        inbound: Receiver<Vec<u8>>,
        outbound: TunnelSend,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Creates the offer (the caller).
    Offerer,
    /// Waits for the offer (the callee).
    Answerer,
}

#[derive(Debug, Clone)]
pub struct CallConfig {
    pub role: Role,
    /// Local UDP address; port 0 = ephemeral, 0.0.0.0 = all interfaces.
    pub bind: SocketAddr,
    /// Host candidate IP when `bind` is unspecified (default: default-route address).
    pub advertise: Option<IpAddr>,
    /// Give up if ICE + DTLS do not complete in this time.
    pub connect_timeout: Duration,
    /// Give up if a lost connection does not come back in this time.
    pub reconnect_timeout: Duration,
    /// No datagram from the remote for this long counts as a lost connection (RTCP
    /// arrives at least every second, audio every 20 ms). ICE consent alone did not
    /// notice a vanished offerer on the controlled side.
    pub silence_timeout: Duration,
    /// Test impairment: share of outgoing datagrams dropped.
    pub send_loss: f32,
    /// STUN servers (`host:port`) asked for our public address before the SDP is
    /// created; empty = host candidates only (one LAN).
    pub stun_servers: Vec<String>,
    /// Longest wait for a STUN answer (delays the offer/answer by at most this).
    pub stun_timeout: Duration,
}

impl CallConfig {
    pub fn new(role: Role, bind: SocketAddr) -> Self {
        Self {
            role,
            bind,
            advertise: None,
            connect_timeout: Duration::from_secs(10),
            reconnect_timeout: Duration::from_secs(10),
            silence_timeout: Duration::from_secs(3),
            send_loss: 0.0,
            stun_servers: Vec::new(),
            stun_timeout: Duration::from_millis(1500),
        }
    }
}

/// One encoded 20 ms frame to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioOut {
    pub seq: u16,
    /// RTP timestamp, 48 kHz units.
    pub timestamp: u32,
    pub payload: Vec<u8>,
}

/// One received Opus packet.
#[derive(Debug, Clone)]
pub struct AudioIn {
    /// RTP stream the packet came on (one per remote member through the SFU).
    pub ssrc: u32,
    pub seq: u16,
    pub timestamp: u32,
    pub payload: Vec<u8>,
    /// When the datagram arrived.
    pub arrival: Instant,
}

/// Call state machine (spec §6, transport part).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallState {
    /// Offerer: offer created; answerer: waiting for the offer.
    Negotiating,
    /// SDP exchanged, ICE/DTLS in progress.
    Connecting,
    Connected,
    /// Connection lost (ICE consent failed); waiting for it to recover.
    Reconnecting,
    /// Ended locally.
    Closed,
    Failed(String),
}

impl CallState {
    pub fn is_final(&self) -> bool {
        matches!(self, CallState::Closed | CallState::Failed(_))
    }
}

/// SDP / candidate text the signaling layer must relay to the remote peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalSignal {
    Offer(String),
    Answer(String),
    Candidate(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum CallEvent {
    Signal(LocalSignal),
    State(CallState),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CallStats {
    pub packets_sent: u64,
    pub packets_received: u64,
    /// Frames from the engine dropped because the call was not connected.
    pub frames_not_connected: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    /// Datagrams dropped by the test impairment.
    pub impaired_drops: u64,
    pub rtt_ms: Option<f64>,
    /// Fraction of our packets the remote reported lost (RTCP), 0–1.
    pub egress_loss: Option<f32>,
    pub ingress_loss: Option<f32>,
    pub connected_after_ms: Option<u64>,
    pub remote_addr: Option<SocketAddr>,
    /// Our public address as a STUN server saw it (server-reflexive candidate).
    pub reflexive_addr: Option<SocketAddr>,
    /// Longest gap between two loop iterations (thread starvation shows up here).
    pub max_loop_gap_ms: f64,
}

enum Command {
    RemoteOffer(String),
    RemoteAnswer(String),
    RemoteCandidate(String),
    ExpectSource(u32),
    ForgetSource(u32),
    Stop,
}

/// Handle to a running call. Dropping it ends the call and joins the thread.
pub struct CallDriver {
    commands: SyncSender<Command>,
    stats: Arc<Mutex<CallStats>>,
    local_addr: SocketAddr,
    thread: Option<JoinHandle<()>>,
}

impl CallDriver {
    /// Binds the socket and starts the call thread. `outgoing` is drained every loop
    /// iteration (≤ 2 ms); `on_audio` is called on the call thread for every received
    /// packet and must not block.
    pub fn start(
        config: CallConfig,
        outgoing: Receiver<AudioOut>,
        on_audio: impl FnMut(AudioIn) + Send + 'static,
    ) -> Result<(CallDriver, Receiver<CallEvent>), TransportError> {
        Self::start_with(config, MediaLink::Udp, outgoing, on_audio)
    }

    /// [`CallDriver::start`] over an explicit [`MediaLink`] (no STUN on a tunnel).
    pub fn start_with(
        config: CallConfig,
        link: MediaLink,
        outgoing: Receiver<AudioOut>,
        on_audio: impl FnMut(AudioIn) + Send + 'static,
    ) -> Result<(CallDriver, Receiver<CallEvent>), TransportError> {
        let rtc = Rtc::builder()
            .set_rtp_mode(true)
            .clear_codecs()
            .enable_opus(true, false)
            .set_stats_interval(Some(Duration::from_secs(1)));
        let mut peer = match link {
            MediaLink::Udp => UdpPeer::with_config(config.bind, config.advertise, rtc)?,
            MediaLink::Tunnel { inbound, outbound } => UdpPeer::tunnel(inbound, outbound, rtc)?,
        };
        if config.send_loss > 0.0 {
            peer.set_send_loss(config.send_loss, 0x5eed);
        }
        let local_addr = peer.local_addr();
        // Room for a whole room's sources announced at once (≤ 100 members).
        let (cmd_tx, cmd_rx) = sync_channel(256);
        let (ev_tx, ev_rx) = sync_channel(64);
        let stats = Arc::new(Mutex::new(CallStats::default()));
        let worker = Worker {
            peer,
            config,
            commands: cmd_rx,
            events: ev_tx,
            outgoing,
            on_audio: Box::new(on_audio),
            stats: stats.clone(),
            st: CallStats::default(),
            state: CallState::Negotiating,
            pending_offer: None,
            mid: None,
            pt: None,
            started: Instant::now(),
            lost_at: None,
            transport_up: false,
            tx_seq: None,
            last_rx: Instant::now(),
            rx_bytes_seen: 0,
            sources: HashSet::new(),
            sources_pending: Vec::new(),
        };
        let thread = std::thread::Builder::new()
            .name("tp-call".into())
            .spawn(move || worker.run())?;
        Ok((
            CallDriver {
                commands: cmd_tx,
                stats,
                local_addr,
                thread: Some(thread),
            },
            ev_rx,
        ))
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    fn send(&self, cmd: Command) -> Result<(), TransportError> {
        self.commands.try_send(cmd).map_err(|e| match e {
            TrySendError::Full(_) => TransportError::Call("command queue full".into()),
            TrySendError::Disconnected(_) => TransportError::Call("call ended".into()),
        })
    }

    /// Answerer: the caller's offer arrived through signaling.
    pub fn remote_offer(&self, sdp: String) -> Result<(), TransportError> {
        self.send(Command::RemoteOffer(sdp))
    }

    /// Offerer: the callee's answer arrived through signaling.
    pub fn remote_answer(&self, sdp: String) -> Result<(), TransportError> {
        self.send(Command::RemoteAnswer(sdp))
    }

    pub fn remote_candidate(&self, candidate: String) -> Result<(), TransportError> {
        self.send(Command::RemoteCandidate(candidate))
    }

    /// Accept the remote RTP stream with this SSRC (packets of unknown SSRCs are dropped).
    /// May be called before the call is connected.
    pub fn expect_source(&self, ssrc: u32) -> Result<(), TransportError> {
        self.send(Command::ExpectSource(ssrc))
    }

    pub fn forget_source(&self, ssrc: u32) -> Result<(), TransportError> {
        self.send(Command::ForgetSource(ssrc))
    }

    pub fn stats(&self) -> CallStats {
        self.stats.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Ends the call, closes the socket and waits for the thread.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(t) = self.thread.take() {
            let _ = self.commands.send(Command::Stop);
            let _ = t.join();
        }
    }
}

impl Drop for CallDriver {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Worker {
    peer: UdpPeer,
    config: CallConfig,
    commands: Receiver<Command>,
    events: SyncSender<CallEvent>,
    outgoing: Receiver<AudioOut>,
    on_audio: Box<dyn FnMut(AudioIn) + Send>,
    stats: Arc<Mutex<CallStats>>,
    st: CallStats,
    state: CallState,
    pending_offer: Option<SdpPendingOffer>,
    mid: Option<Mid>,
    pt: Option<Pt>,
    started: Instant,
    /// When the connection was lost (Reconnecting).
    lost_at: Option<Instant>,
    /// ICE + DTLS are up (`Event::Connected`); the call is Connected once the audio
    /// m-line is known too (str0m reports `MediaAdded` after `Connected`).
    transport_up: bool,
    /// Last extended (64-bit) RTP sequence number written; str0m needs extended numbers.
    tx_seq: Option<u64>,
    /// Last time any datagram arrived (bytes counter moved).
    last_rx: Instant,
    rx_bytes_seen: u64,
    /// Remote SSRCs to accept; `sources_pending` waits until the audio m-line exists.
    sources: HashSet<u32>,
    sources_pending: Vec<u32>,
}

/// Longest idle sleep of the call loop: bounds the extra latency of an outgoing frame.
const STEP: Duration = Duration::from_millis(1);

impl Worker {
    fn run(mut self) {
        // Before any SDP: the reflexive candidate travels inside the offer/answer. A
        // remote offer waits in the command queue meanwhile.
        if !self.config.stun_servers.is_empty() && !self.peer.is_tunnel() {
            let servers: Vec<SocketAddr> = self
                .config
                .stun_servers
                .iter()
                .filter_map(|s| s.to_socket_addrs().ok())
                .flatten()
                .collect();
            self.st.reflexive_addr = self
                .peer
                .add_server_reflexive(&servers, self.config.stun_timeout);
            self.publish();
        }
        if self.config.role == Role::Offerer
            && let Err(e) = self.create_offer()
        {
            self.fail(e.to_string());
        }
        let mut last_publish = Instant::now();
        let mut last_iter = Instant::now();
        while !self.state.is_final() {
            let gap = last_iter.elapsed().as_secs_f64() * 1000.0;
            self.st.max_loop_gap_ms = self.st.max_loop_gap_ms.max(gap);
            last_iter = Instant::now();
            if !self.handle_commands() {
                self.set_state(CallState::Closed);
                break;
            }
            self.send_audio();
            self.declare_sources();
            match self.peer.step(STEP) {
                Ok(events) => {
                    for e in events {
                        self.handle_event(e);
                    }
                }
                Err(e) => self.fail(format!("transport: {e}")),
            }
            self.check_timeouts();
            if last_publish.elapsed() >= Duration::from_millis(100) {
                self.publish();
                last_publish = Instant::now();
            }
        }
        self.peer.rtc.disconnect();
        self.publish();
    }

    fn emit(&self, ev: CallEvent) {
        // The owner reads events; if it stopped reading, dropping them is the only
        // option that does not block the call thread.
        let _ = self.events.try_send(ev);
    }

    fn set_state(&mut self, state: CallState) {
        if self.state != state {
            self.state = state.clone();
            self.emit(CallEvent::State(state));
        }
    }

    fn fail(&mut self, reason: String) {
        self.set_state(CallState::Failed(reason));
    }

    fn create_offer(&mut self) -> Result<(), TransportError> {
        let mut change = self.peer.rtc.sdp_api();
        let mid = change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
        let (offer, pending) = change
            .apply()
            .ok_or_else(|| TransportError::Call("empty offer".into()))?;
        self.mid = Some(mid);
        self.pending_offer = Some(pending);
        self.emit(CallEvent::Signal(LocalSignal::Offer(offer.to_sdp_string())));
        Ok(())
    }

    /// Returns `false` on stop.
    fn handle_commands(&mut self) -> bool {
        loop {
            let cmd = match self.commands.try_recv() {
                Ok(c) => c,
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => return false,
            };
            let result = match cmd {
                Command::Stop => return false,
                Command::RemoteOffer(sdp) => self.accept_offer(&sdp),
                Command::RemoteAnswer(sdp) => self.accept_answer(&sdp),
                Command::RemoteCandidate(c) => Candidate::from_sdp_string(&c)
                    .map(|c| self.peer.rtc.add_remote_candidate(c))
                    .map_err(|e| TransportError::Candidate(e.to_string())),
                Command::ExpectSource(ssrc) => {
                    if self.sources.insert(ssrc) {
                        self.sources_pending.push(ssrc);
                    }
                    Ok(())
                }
                Command::ForgetSource(ssrc) => {
                    if self.sources.remove(&ssrc) {
                        self.sources_pending.retain(|s| *s != ssrc);
                        self.peer
                            .rtc
                            .direct_api()
                            .remove_stream_rx(Ssrc::from(ssrc));
                    }
                    Ok(())
                }
            };
            if let Err(e) = result {
                self.fail(format!("signaling: {e}"));
            }
        }
    }

    fn accept_offer(&mut self, sdp: &str) -> Result<(), TransportError> {
        if self.config.role != Role::Answerer {
            return Err(TransportError::Call("offer received by the offerer".into()));
        }
        let offer = SdpOffer::from_sdp_string(sdp)
            .map_err(|e| TransportError::Call(format!("bad offer: {e}")))?;
        let answer = self.peer.rtc.sdp_api().accept_offer(offer)?;
        self.emit(CallEvent::Signal(LocalSignal::Answer(
            answer.to_sdp_string(),
        )));
        self.set_state(CallState::Connecting);
        Ok(())
    }

    fn accept_answer(&mut self, sdp: &str) -> Result<(), TransportError> {
        let pending = self
            .pending_offer
            .take()
            .ok_or_else(|| TransportError::Call("unexpected answer".into()))?;
        let answer = SdpAnswer::from_sdp_string(sdp)
            .map_err(|e| TransportError::Call(format!("bad answer: {e}")))?;
        self.peer.rtc.sdp_api().accept_answer(pending, answer)?;
        self.set_state(CallState::Connecting);
        Ok(())
    }

    /// Registers pending remote SSRCs once the audio m-line exists.
    fn declare_sources(&mut self) {
        if self.sources_pending.is_empty() {
            return;
        }
        let Some(mid) = self.mid.filter(|m| self.peer.rtc.media(*m).is_some()) else {
            return;
        };
        let mut api = self.peer.rtc.direct_api();
        for ssrc in self.sources_pending.drain(..) {
            api.expect_stream_rx(Ssrc::from(ssrc), None, mid, None);
        }
    }

    fn send_audio(&mut self) {
        loop {
            let frame = match self.outgoing.try_recv() {
                Ok(f) => f,
                Err(_) => return,
            };
            let (Some(mid), Some(pt)) = (self.mid, self.pt) else {
                self.st.frames_not_connected += 1;
                continue;
            };
            if self.state != CallState::Connected {
                self.st.frames_not_connected += 1;
                continue;
            }
            let ext = extend_seq(self.tx_seq, frame.seq);
            self.tx_seq = Some(ext);
            let write = RtpWrite::new(
                pt,
                SeqNo::from(ext),
                frame.timestamp,
                Instant::now(),
                frame.payload,
            );
            let mut api = self.peer.rtc.direct_api();
            let Some(tx) = api.stream_tx_by_mid(mid, None) else {
                self.st.frames_not_connected += 1;
                continue;
            };
            tx.write_rtp(write);
            self.st.packets_sent += 1;
        }
    }

    fn handle_event(&mut self, e: Event) {
        match e {
            Event::Connected => {
                self.transport_up = true;
                self.try_ready();
            }
            Event::IceConnectionStateChange(IceConnectionState::Disconnected)
                if self.state == CallState::Connected =>
            {
                self.lost_at = Some(Instant::now());
                self.set_state(CallState::Reconnecting);
            }
            Event::IceConnectionStateChange(
                IceConnectionState::Connected | IceConnectionState::Completed,
            ) if self.state == CallState::Reconnecting => {
                self.lost_at = None;
                self.set_state(CallState::Connected);
            }
            Event::MediaAdded(m) => {
                if m.kind == MediaKind::Audio {
                    self.mid = Some(m.mid);
                    self.try_ready();
                }
            }
            Event::RtpPacket(p) => {
                self.st.packets_received += 1;
                if self.state == CallState::Reconnecting {
                    self.lost_at = None;
                    self.set_state(CallState::Connected);
                }
                (self.on_audio)(AudioIn {
                    ssrc: *p.header.ssrc,
                    // Truncating the extended number gives the sender's u16 back.
                    seq: *p.seq_no as u16,
                    timestamp: p.header.timestamp,
                    payload: p.payload.to_vec(),
                    arrival: p.timestamp,
                });
            }
            Event::PeerStats(s) => {
                self.st.rtt_ms = s.rtt.map(|d| d.as_secs_f64() * 1000.0);
                self.st.egress_loss = s.egress_loss_fraction;
                self.st.ingress_loss = s.ingress_loss_fraction;
                self.st.remote_addr = s.selected_candidate_pair.as_ref().map(|p| p.remote.addr);
            }
            _ => {}
        }
    }

    fn try_ready(&mut self) {
        if !self.transport_up
            || !matches!(self.state, CallState::Negotiating | CallState::Connecting)
        {
            return;
        }
        if self.pt.is_none() {
            self.pt = self
                .peer
                .rtc
                .codec_config()
                .find(|p| p.spec().codec == Codec::Opus)
                .map(|p| p.pt());
        }
        if self.pt.is_none() {
            self.fail("no Opus codec negotiated".into());
            return;
        }
        if self.mid.is_some() {
            self.st.connected_after_ms = Some(self.started.elapsed().as_millis() as u64);
            self.set_state(CallState::Connected);
        }
    }

    fn check_timeouts(&mut self) {
        if self.peer.bytes_received != self.rx_bytes_seen {
            self.rx_bytes_seen = self.peer.bytes_received;
            self.last_rx = Instant::now();
        }
        if self.state == CallState::Connected
            && self.last_rx.elapsed() > self.config.silence_timeout
        {
            self.lost_at = Some(self.last_rx);
            self.set_state(CallState::Reconnecting);
        }
        let waiting = matches!(self.state, CallState::Negotiating | CallState::Connecting);
        if waiting && self.started.elapsed() > self.config.connect_timeout {
            self.fail("connect timeout".into());
        }
        if let Some(t) = self.lost_at
            && t.elapsed() > self.config.reconnect_timeout
        {
            self.fail("connection lost".into());
        }
        if !self.peer.rtc.is_alive() && !self.state.is_final() {
            self.fail("peer connection closed".into());
        }
    }

    fn publish(&mut self) {
        self.st.bytes_sent = self.peer.bytes_sent;
        self.st.bytes_received = self.peer.bytes_received;
        self.st.impaired_drops = self.peer.datagrams_dropped;
        if let Ok(mut s) = self.stats.lock() {
            s.clone_from(&self.st);
        }
    }
}

/// Extends a 16-bit sequence number relative to the previous extended one (nearest
/// value, so reordering and wrap-around both work). The upper bits are the SRTP rollover
/// counter, which the receiver assumes starts at 0: the first number is taken as is.
pub fn extend_seq(prev: Option<u64>, seq: u16) -> u64 {
    match prev {
        None => u64::from(seq),
        Some(p) => {
            let delta = i64::from(seq.wrapping_sub(p as u16) as i16);
            p.saturating_add_signed(delta)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::extend_seq;

    #[test]
    fn sequence_numbers_extend_across_wrap_and_reordering() {
        let a = extend_seq(None, 65_534);
        let b = extend_seq(Some(a), 65_535);
        let c = extend_seq(Some(b), 0);
        let d = extend_seq(Some(c), 1);
        assert_eq!([b - a, c - b, d - c], [1, 1, 1]);
        // An older packet after the wrap goes back, not forward by 65 535.
        assert_eq!(extend_seq(Some(d), 65_535), d - 2);
        assert_eq!(extend_seq(Some(d), 11), d + 10);
    }
}
