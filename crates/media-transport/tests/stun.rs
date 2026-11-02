//! Server-reflexive candidates: a fake STUN server on loopback answers with a "public"
//! address; it must show up in the SDP as `typ srflx` and in the stats, and the call must
//! still connect (over the host candidates here, there is no NAT on loopback). A STUN
//! server that never answers only delays the offer by the STUN timeout.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::{Duration, Instant};

use turbo_pancake_media_transport::call::{
    AudioOut, CallConfig, CallDriver, CallEvent, CallState, LocalSignal, Role,
};

const PUBLIC: &str = "203.0.113.7:40000";

/// Answers Binding requests with XOR-MAPPED-ADDRESS = `PUBLIC`.
fn fake_stun_server() -> SocketAddr {
    let sock = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = sock.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 1500];
        let public: SocketAddr = PUBLIC.parse().unwrap();
        let SocketAddr::V4(v4) = public else {
            unreachable!()
        };
        while let Ok((n, from)) = sock.recv_from(&mut buf) {
            if n < 20 || buf[0..2] != [0x00, 0x01] {
                continue;
            }
            let mut r = vec![0x01, 0x01, 0x00, 0x0c];
            r.extend_from_slice(&buf[4..20]); // cookie + transaction id
            let port = v4.port() ^ 0x2112;
            let ip = u32::from(*v4.ip()) ^ 0x2112_A442;
            r.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, 0x01]);
            r.extend_from_slice(&port.to_be_bytes());
            r.extend_from_slice(&ip.to_be_bytes());
            let _ = sock.send_to(&r, from);
        }
    });
    addr
}

fn start(role: Role, stun: Vec<String>) -> (CallDriver, Receiver<CallEvent>) {
    // No audio: the driver treats a closed frame channel as "nothing to send".
    let (_, rx) = sync_channel::<AudioOut>(4);
    let mut cfg = CallConfig::new(role, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    cfg.stun_servers = stun;
    cfg.stun_timeout = Duration::from_millis(600);
    CallDriver::start(cfg, rx, |_| {}).unwrap()
}

/// Relays signals until both are connected; returns the offer and answer SDP.
fn connect(
    a: &(CallDriver, Receiver<CallEvent>),
    b: &(CallDriver, Receiver<CallEvent>),
) -> (String, String) {
    let (mut offer, mut answer) = (String::new(), String::new());
    let (mut a_up, mut b_up) = (false, false);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(a_up && b_up) {
        assert!(Instant::now() < deadline, "not connected");
        while let Ok(ev) = a.1.try_recv() {
            match ev {
                CallEvent::Signal(LocalSignal::Offer(s)) => {
                    offer = s.clone();
                    b.0.remote_offer(s).unwrap();
                }
                CallEvent::Signal(LocalSignal::Candidate(c)) => b.0.remote_candidate(c).unwrap(),
                CallEvent::State(CallState::Connected) => a_up = true,
                CallEvent::State(CallState::Failed(e)) => panic!("a failed: {e}"),
                _ => {}
            }
        }
        while let Ok(ev) = b.1.try_recv() {
            match ev {
                CallEvent::Signal(LocalSignal::Answer(s)) => {
                    answer = s.clone();
                    a.0.remote_answer(s).unwrap();
                }
                CallEvent::Signal(LocalSignal::Candidate(c)) => a.0.remote_candidate(c).unwrap(),
                CallEvent::State(CallState::Connected) => b_up = true,
                CallEvent::State(CallState::Failed(e)) => panic!("b failed: {e}"),
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    (offer, answer)
}

#[test]
fn reflexive_candidate_in_sdp_and_call_connects() {
    let stun = fake_stun_server().to_string();
    let a = start(Role::Offerer, vec![stun.clone()]);
    let b = start(Role::Answerer, vec![stun]);
    let (offer, answer) = connect(&a, &b);
    for sdp in [&offer, &answer] {
        assert!(
            sdp.lines()
                .any(|l| l.contains("typ srflx") && l.contains("203.0.113.7 40000")),
            "no srflx candidate in\n{sdp}"
        );
    }
    let public: SocketAddr = PUBLIC.parse().unwrap();
    assert_eq!(a.0.stats().reflexive_addr, Some(public));
    assert_eq!(b.0.stats().reflexive_addr, Some(public));
    a.0.stop();
    b.0.stop();
}

#[test]
fn silent_stun_server_only_delays_the_offer() {
    // Bound but never answering.
    let silent = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let stun = vec![
        silent.local_addr().unwrap().to_string(),
        "no.such.host.invalid:3478".into(),
    ];
    let t = Instant::now();
    let a = start(Role::Offerer, stun.clone());
    let b = start(Role::Answerer, stun);
    let (offer, _) = connect(&a, &b);
    assert!(!offer.contains("srflx"));
    assert_eq!(a.0.stats().reflexive_addr, None);
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    a.0.stop();
    b.0.stop();
}
