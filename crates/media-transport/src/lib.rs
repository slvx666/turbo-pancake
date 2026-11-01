//! WebRTC transport built on `str0m` (Sans-I/O).
//!
//! S0 contains only the pieces needed to prove feasibility: a UDP-driven peer
//! wrapper that owns one socket and one `Rtc`, used by the loopback spike test
//! (`tests/loopback_spike.rs`). It is not yet wired into the desktop client or
//! an SFU; that is S1/S2 work.

use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Once;
use std::time::{Duration, Instant};

pub use str0m;
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcError};

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("rtc: {0}")]
    Rtc(#[from] RtcError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("ice candidate: {0}")]
    Candidate(String),
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
}

impl UdpPeer {
    /// Binds `bind` (port 0 = ephemeral) and registers it as a host candidate.
    pub fn bind(bind: SocketAddr) -> Result<Self, TransportError> {
        init_crypto();
        let socket = UdpSocket::bind(bind)?;
        let local = socket.local_addr()?;
        let mut rtc = Rtc::builder().build(Instant::now());
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
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Flushes pending output, then waits at most `max_wait` for one datagram
    /// and feeds it (and the clock) into the `Rtc`. Returns emitted events.
    pub fn step(&mut self, max_wait: Duration) -> Result<Vec<Event>, TransportError> {
        let mut events = Vec::new();
        self.drain(&mut events)?;

        let wait = self
            .next_timeout
            .saturating_duration_since(Instant::now())
            .clamp(
                Duration::from_millis(1),
                max_wait.max(Duration::from_millis(1)),
            );
        self.socket.set_read_timeout(Some(wait))?;
        match self.socket.recv_from(&mut self.buf) {
            Ok((n, source)) => {
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
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            // Windows reports ICMP port-unreachable from a previous send as a recv error.
            Err(e) if e.kind() == ErrorKind::ConnectionReset => {}
            Err(e) => return Err(e.into()),
        }
        self.rtc.handle_input(Input::Timeout(Instant::now()))?;
        self.drain(&mut events)?;
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
                    self.bytes_sent += t.contents.len() as u64;
                    self.socket.send_to(&t.contents, t.destination)?;
                }
                Output::Event(e) => events.push(e),
            }
        }
    }
}
