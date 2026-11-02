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
//! S1 scope: one forwarded audio stream per member. In a room with more than two members
//! each member hears the earliest other member; per-member tracks come with S2.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use str0m::change::SdpOffer;
use str0m::format::Codec;
use str0m::media::{MediaKind, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::rtp::{RtpWrite, SeqNo};
use str0m::{Candidate, Event, Input, Output, Rtc};
use turbo_pancake_media_transport::call::extend_seq;
use turbo_pancake_media_transport::{TUNNEL_CLIENT, TUNNEL_SERVER, init_crypto};

pub type PeerId = String;

/// Where the SFU delivers what belongs to one member. Called on the SFU thread: must not
/// block (drop datagrams when the member's connection is full).
pub trait PeerSink: Send + 'static {
    /// The SDP answer to the member's offer, or why it was rejected.
    fn answer(&mut self, result: Result<String, String>);
    /// One datagram for a [`Link::Tunnel`] member.
    fn datagram(&mut self, data: Vec<u8>);
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
    pub udp_in: u64,
    pub udp_out: u64,
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
            order: 0,
            buf: vec![0; 2048],
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
    connected: bool,
    next_timeout: Instant,
    /// Join order: in rooms of more than two, everyone hears the earliest other member.
    order: u64,
    /// Outgoing sequence numbering: extended number of the current source's stream and the
    /// offset that keeps our numbers monotonic when the source changes (SRTP index).
    src: Option<PeerId>,
    src_ext: Option<u64>,
    offset: i64,
    last_out: Option<u64>,
}

struct Forward {
    from: PeerId,
    room: String,
    seq: u16,
    timestamp: u32,
    payload: Vec<u8>,
}

struct Worker {
    socket: Option<UdpSocket>,
    udp_port: u16,
    cmds: Receiver<Cmd>,
    peers: HashMap<PeerId, Member>,
    order: u64,
    buf: Vec<u8>,
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
            let forwards = self.drain();
            if !forwards.is_empty() {
                busy = true;
                self.forward(forwards);
                // Send what forwarding queued right away.
                let more = self.drain();
                self.forward(more);
            }
            self.peers.retain(|_, m| m.rtc.is_alive());
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
                        self.order += 1;
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
                                connected: false,
                                next_timeout: Instant::now(),
                                order: self.order,
                                src: None,
                                src_ext: None,
                                offset: 0,
                                last_out: None,
                            },
                        );
                    }
                    Err(e) => sink.answer(Err(e)),
                }
            }
            Cmd::Leave(peer) => self.remove(&peer),
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
                    }
                }
            }
        }
        true
    }

    /// Drops a member's session. Its listeners restart their numbering on the next
    /// source: a new session of the same member starts a new sequence space.
    fn remove(&mut self, peer: &str) {
        if let Some(mut m) = self.peers.remove(peer) {
            m.rtc.disconnect();
        }
        for m in self.peers.values_mut() {
            if m.src.as_deref() == Some(peer) {
                m.src = None;
            }
        }
    }

    fn accept(
        &self,
        _room: &str,
        offer: &str,
        link: &Link,
    ) -> Result<(Rtc, String, SocketAddr), String> {
        let mut rtc = Rtc::builder()
            .set_ice_lite(true)
            .set_rtp_mode(true)
            .clear_codecs()
            .enable_opus(true, false)
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
            for m in self.peers.values_mut().filter(|m| !m.tunnel) {
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
                    break;
                }
            }
        }
        any
    }

    /// Sends everything the members' Rtcs want to send and collects received RTP.
    fn drain(&mut self) -> Vec<Forward> {
        let mut forwards = Vec::new();
        for (id, m) in self.peers.iter_mut() {
            loop {
                match m.rtc.poll_output() {
                    Ok(Output::Timeout(t)) => {
                        m.next_timeout = t;
                        break;
                    }
                    Ok(Output::Transmit(t)) => {
                        if m.tunnel {
                            self.st.tunnel_out += 1;
                            m.sink.datagram(t.contents.to_vec());
                        } else if let Some(s) = &self.socket {
                            self.st.udp_out += 1;
                            let _ = s.send_to(&t.contents, t.destination);
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
                        }
                        Event::MediaAdded(a) if a.kind == MediaKind::Audio => m.mid = Some(a.mid),
                        Event::RtpPacket(p) => {
                            self.st.rtp_in += 1;
                            forwards.push(Forward {
                                from: id.clone(),
                                room: m.room.clone(),
                                seq: *p.seq_no as u16,
                                timestamp: p.header.timestamp,
                                payload: p.payload.to_vec(),
                            });
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
        forwards
    }

    fn forward(&mut self, forwards: Vec<Forward>) {
        for f in forwards {
            // Earliest other member of each target's room is its source.
            let targets: Vec<PeerId> = self
                .peers
                .iter()
                .filter(|(id, m)| **id != f.from && m.room == f.room)
                .filter(|(id, _)| self.source_of(id).as_deref() == Some(f.from.as_str()))
                .map(|(id, _)| id.clone())
                .collect();
            for t in targets {
                let Some(m) = self.peers.get_mut(&t) else {
                    continue;
                };
                let (Some(mid), Some(pt), true) = (m.mid, m.pt, m.connected) else {
                    continue;
                };
                if m.src.as_deref() != Some(f.from.as_str()) {
                    // New source: continue our numbering after the last packet sent.
                    m.src = Some(f.from.clone());
                    m.src_ext = None;
                }
                let ext = extend_seq(m.src_ext, f.seq);
                if m.src_ext.is_none() {
                    m.offset = m.last_out.map_or(0, |l| l as i64 + 1 - ext as i64);
                }
                m.src_ext = Some(ext);
                let out = (ext as i64 + m.offset).max(0) as u64;
                m.last_out = Some(m.last_out.map_or(out, |l| l.max(out)));
                let write = RtpWrite::new(
                    pt,
                    SeqNo::from(out),
                    f.timestamp,
                    Instant::now(),
                    f.payload.clone(),
                );
                if let Some(tx) = m.rtc.direct_api().stream_tx_by_mid(mid, None) {
                    tx.write_rtp(write);
                    self.st.rtp_out += 1;
                }
            }
        }
    }

    fn source_of(&self, target: &str) -> Option<PeerId> {
        let room = &self.peers.get(target)?.room;
        self.peers
            .iter()
            .filter(|(id, m)| id.as_str() != target && &m.room == room)
            .min_by_key(|(_, m)| m.order)
            .map(|(id, _)| id.clone())
    }

    fn publish(&mut self) {
        self.st.peers = self.peers.len();
        self.st.tunnel_peers = self.peers.values().filter(|m| m.tunnel).count();
        self.st.connected_peers = self.peers.values().filter(|m| m.connected).count();
        if let Ok(mut s) = self.stats.lock() {
            let dropped = s.dropped_commands;
            *s = self.st.clone();
            s.dropped_commands = dropped;
        }
    }
}
