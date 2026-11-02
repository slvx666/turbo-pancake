//! WebRTC transport built on `str0m` (Sans-I/O).
//!
//! [`UdpPeer`] owns one UDP socket and one `Rtc` and moves datagrams between them.
//! [`call::CallDriver`] runs one peer on its own thread for a 1:1 voice call (S1):
//! SDP offer/answer, an Opus track in RTP mode (our jitter buffer does the reordering),
//! bounded channels to and from the audio engine, and call state.

pub mod call;

use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Once;
use std::time::{Duration, Instant};

pub use str0m;
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcConfig, RtcError};

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("rtc: {0}")]
    Rtc(#[from] RtcError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("ice candidate: {0}")]
    Candidate(String),
    #[error("call: {0}")]
    Call(String),
}

static CRYPTO: Once = Once::new();

/// Installs the process-wide crypto provider exactly once.
pub fn init_crypto() {
    CRYPTO.call_once(|| str0m::crypto::from_feature_flags().install_process_default());
}

/// One WebRTC endpoint bound to one UDP socket.
pub struct UdpPeer {
    pub rtc: Rtc,
    socket: UdpSocket,
    local: SocketAddr,
    buf: Vec<u8>,
    next_timeout: Instant,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    /// Test impairment: share of outgoing datagrams dropped (deterministic PRNG).
    send_loss: f32,
    rng: u64,
    pub datagrams_dropped: u64,
}

impl UdpPeer {
    /// Binds `bind` (port 0 = ephemeral) and registers it as a host candidate.
    pub fn bind(bind: SocketAddr) -> Result<Self, TransportError> {
        Self::with_config(bind, None, Rtc::builder())
    }

    /// Like [`UdpPeer::bind`] with an explicit `Rtc` configuration. When `bind` is an
    /// unspecified address (0.0.0.0), `advertise` is the IP announced as host candidate
    /// (default: the address of the default route, see [`default_route_ip`]).
    pub fn with_config(
        bind: SocketAddr,
        advertise: Option<IpAddr>,
        config: RtcConfig,
    ) -> Result<Self, TransportError> {
        init_crypto();
        let socket = UdpSocket::bind(bind)?;
        socket.set_nonblocking(true)?;
        let mut local = socket.local_addr()?;
        if local.ip().is_unspecified() {
            let ip = match advertise {
                Some(ip) => ip,
                None => default_route_ip(local.is_ipv6())?,
            };
            local.set_ip(ip);
        }
        let mut rtc = config.build(Instant::now());
        let candidate =
            Candidate::host(local, "udp").map_err(|e| TransportError::Candidate(e.to_string()))?;
        rtc.add_local_candidate(candidate);
        Ok(Self {
            rtc,
            socket,
            local,
            buf: vec![0; 2000],
            next_timeout: Instant::now(),
            bytes_sent: 0,
            bytes_received: 0,
            send_loss: 0.0,
            rng: 0x9e37_79b9_7f4a_7c15,
            datagrams_dropped: 0,
        })
    }

    /// Drops `loss` (0–1) of all outgoing datagrams, for tests over real UDP.
    pub fn set_send_loss(&mut self, loss: f32, seed: u64) {
        self.send_loss = loss.clamp(0.0, 1.0);
        self.rng = seed.max(1);
    }

    fn drop_this_one(&mut self) -> bool {
        if self.send_loss <= 0.0 {
            return false;
        }
        // xorshift64*
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        let unit = (x.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 40) as f32 / (1u64 << 24) as f32;
        unit < self.send_loss
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Flushes pending output, feeds every datagram already queued on the socket (and
    /// the clock) into the `Rtc`, and returns the emitted events. When nothing arrived it
    /// then sleeps until the next `Rtc` timeout, at most `max_wait`.
    ///
    /// The socket is non-blocking and the wait is a `thread::sleep`: a socket read
    /// timeout of 1–2 ms really waits ~16 ms on Windows (timer resolution), which delayed
    /// outgoing audio and let datagrams pile up (one read per iteration).
    pub fn step(&mut self, max_wait: Duration) -> Result<Vec<Event>, TransportError> {
        let mut events = Vec::new();
        self.drain(&mut events)?;
        let mut received = 0;
        loop {
            match self.socket.recv_from(&mut self.buf) {
                Ok((n, source)) => {
                    received += 1;
                    self.bytes_received += n as u64;
                    if let Ok(contents) = self.buf[..n].try_into() {
                        let input = Input::Receive(
                            Instant::now(),
                            Receive {
                                proto: Protocol::Udp,
                                source,
                                destination: self.local,
                                contents,
                            },
                        );
                        if self.rtc.accepts(&input) {
                            self.rtc.handle_input(input)?;
                        }
                    }
                    // RTP mode keeps one pending packet: poll before the next datagram.
                    self.drain(&mut events)?;
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => break,
                // Windows reports ICMP port-unreachable from a previous send as a recv error.
                Err(e) if e.kind() == ErrorKind::ConnectionReset => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.rtc.handle_input(Input::Timeout(Instant::now()))?;
        self.drain(&mut events)?;
        if received == 0 && events.is_empty() {
            let wait = self
                .next_timeout
                .saturating_duration_since(Instant::now())
                .min(max_wait);
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
        }
        Ok(events)
    }

    fn drain(&mut self, events: &mut Vec<Event>) -> Result<(), TransportError> {
        loop {
            match self.rtc.poll_output()? {
                Output::Timeout(t) => {
                    self.next_timeout = t;
                    return Ok(());
                }
                Output::Transmit(t) => {
                    if self.drop_this_one() {
                        self.datagrams_dropped += 1;
                        continue;
                    }
                    self.bytes_sent += t.contents.len() as u64;
                    match self.socket.send_to(&t.contents, t.destination) {
                        Ok(_) => {}
                        // Unreachable/no route: ICE consent checks will notice; a call
                        // must not die on one failed datagram.
                        Err(e)
                            if matches!(
                                e.kind(),
                                ErrorKind::ConnectionReset
                                    | ErrorKind::NetworkUnreachable
                                    | ErrorKind::HostUnreachable
                                    | ErrorKind::WouldBlock
                            ) => {}
                        Err(e) => return Err(e.into()),
                    }
                }
                Output::Event(e) => events.push(e),
            }
        }
    }
}

/// Local address of the default route: a UDP socket "connected" to a public address
/// (nothing is sent) reports which interface the OS would use.
pub fn default_route_ip(ipv6: bool) -> Result<IpAddr, TransportError> {
    let (bind, probe): (SocketAddr, SocketAddr) = if ipv6 {
        (
            "[::]:0".parse().unwrap(),
            "[2001:4860:4860::8888]:53".parse().unwrap(),
        )
    } else {
        ("0.0.0.0:0".parse().unwrap(), "8.8.8.8:53".parse().unwrap())
    };
    let s = UdpSocket::bind(bind)?;
    s.connect(probe)?;
    Ok(s.local_addr()?.ip())
}
