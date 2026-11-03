//! `CallDriver` over real loopback UDP: offer/answer relayed by the test (as the
//! signaling server would), ICE + DTLS-SRTP, Opus-sized RTP frames at 20 ms cadence in
//! both directions, test loss on the wire, and loss of the remote peer.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use turbo_pancake_media_transport::call::{
    AudioIn, AudioOut, CallConfig, CallDriver, CallEvent, CallState, LocalSignal, Role,
};

fn loopback() -> SocketAddr {
    (Ipv4Addr::LOCALHOST, 0).into()
}

struct Side {
    driver: CallDriver,
    events: Receiver<CallEvent>,
    audio_tx: SyncSender<AudioOut>,
    received: Arc<Mutex<Vec<AudioIn>>>,
    states: Vec<CallState>,
}

fn side(config: CallConfig) -> Side {
    let (audio_tx, audio_rx) = sync_channel(50);
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink = received.clone();
    let (driver, events) =
        CallDriver::start(config, audio_rx, move |p| sink.lock().unwrap().push(p)).unwrap();
    Side {
        driver,
        events,
        audio_tx,
        received,
        states: Vec::new(),
    }
}

/// Moves signals from `from` to `to` and records `from`'s states.
fn relay(from: &mut Side, to: &Side) {
    while let Ok(ev) = from.events.try_recv() {
        match ev {
            CallEvent::Signal(LocalSignal::Offer(sdp)) => to.driver.remote_offer(sdp).unwrap(),
            CallEvent::Signal(LocalSignal::Answer(sdp)) => to.driver.remote_answer(sdp).unwrap(),
            CallEvent::Signal(LocalSignal::Candidate(c)) => to.driver.remote_candidate(c).unwrap(),
            CallEvent::State(s) => from.states.push(s),
            CallEvent::KeyframeRequested | CallEvent::PreviewKeyframeRequested => {}
        }
    }
}

/// Relays both ways. Returns true when both sides are connected.
fn pump(a: &mut Side, b: &mut Side) -> bool {
    relay(a, b);
    relay(b, a);
    a.states.last() == Some(&CallState::Connected) && b.states.last() == Some(&CallState::Connected)
}

fn connect(a_cfg: CallConfig, b_cfg: CallConfig) -> (Side, Side) {
    let mut a = side(a_cfg);
    let mut b = side(b_cfg);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !pump(&mut a, &mut b) {
        assert!(
            Instant::now() < deadline,
            "not connected: a {:?} b {:?}",
            a.states,
            b.states
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    (a, b)
}

/// Opus-sized fake frame whose bytes encode its own sequence number.
fn frame(seq: u16) -> AudioOut {
    let mut payload = vec![(seq & 0xff) as u8; 80];
    payload[0] = (seq >> 8) as u8;
    AudioOut {
        seq,
        timestamp: u32::from(seq) * 960,
        payload,
    }
}

/// Sends `n` frames from both sides at real-time 20 ms cadence; seq starts near the
/// u16 wrap to cover it.
fn talk(a: &mut Side, b: &mut Side, n: u16, first: u16) {
    let start = Instant::now();
    for i in 0..n {
        let due = start + Duration::from_millis(20 * u64::from(i));
        if let Some(w) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(w);
        }
        let seq = first.wrapping_add(i);
        a.audio_tx.try_send(frame(seq)).unwrap();
        b.audio_tx.try_send(frame(seq)).unwrap();
        pump(a, b);
    }
    std::thread::sleep(Duration::from_millis(300));
    pump(a, b);
}

fn check_received(got: &[AudioIn], n: u16, first: u16) -> usize {
    for p in got {
        let expect = frame(p.seq);
        assert_eq!(p.payload, expect.payload, "payload of seq {}", p.seq);
        assert_eq!(p.timestamp, expect.timestamp, "timestamp of seq {}", p.seq);
        assert!(p.seq.wrapping_sub(first) < n, "unexpected seq {}", p.seq);
    }
    let mut seqs: Vec<u16> = got.iter().map(|p| p.seq.wrapping_sub(first)).collect();
    seqs.sort_unstable();
    seqs.dedup();
    seqs.len()
}

#[test]
fn audio_flows_both_ways_and_stops_cleanly() {
    let (mut a, mut b) = connect(
        CallConfig::new(Role::Offerer, loopback()),
        CallConfig::new(Role::Answerer, loopback()),
    );
    let first = 65_500; // crosses the u16 wrap after 36 frames
    talk(&mut a, &mut b, 150, first);
    let at_b = check_received(&b.received.lock().unwrap(), 150, first);
    let at_a = check_received(&a.received.lock().unwrap(), 150, first);
    assert!(at_b >= 148, "b got {at_b}/150");
    assert!(at_a >= 148, "a got {at_a}/150");
    let st = a.driver.stats();
    assert!(
        st.packets_sent >= 150 && st.packets_received >= 148,
        "{st:?}"
    );
    assert!(st.connected_after_ms.unwrap() < 5000);
    assert_eq!(st.frames_not_connected, 0);
    eprintln!("lossless: {st:?}");
    a.driver.stop();
    b.driver.stop();
}

#[test]
fn loss_on_the_wire_loses_only_those_packets() {
    let mut cfg = CallConfig::new(Role::Offerer, loopback());
    cfg.send_loss = 0.1;
    let (mut a, mut b) = connect(cfg, CallConfig::new(Role::Answerer, loopback()));
    let first = 0;
    talk(&mut a, &mut b, 250, first);
    let at_b = check_received(&b.received.lock().unwrap(), 250, first);
    let at_a = check_received(&a.received.lock().unwrap(), 250, first);
    eprintln!(
        "10 % loss a→b: b got {at_b}/250, a got {at_a}/250, a stats {:?}",
        a.driver.stats()
    );
    // 10 % of a's datagrams are dropped (binomial, deterministic seed).
    assert!((200..=240).contains(&at_b), "b got {at_b}/250");
    assert!(at_a >= 245, "a got {at_a}/250");
    assert!(a.driver.stats().impaired_drops > 15);
}

#[test]
fn lost_peer_moves_to_reconnecting_then_failed() {
    let mut cfg = CallConfig::new(Role::Offerer, loopback());
    cfg.reconnect_timeout = Duration::from_secs(2);
    let (mut a, mut b) = connect(cfg, CallConfig::new(Role::Answerer, loopback()));
    talk(&mut a, &mut b, 10, 0);
    // The remote process disappears without saying goodbye.
    let Side { driver, .. } = b;
    driver.stop();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        while let Ok(ev) = a.events.try_recv() {
            if let CallEvent::State(s) = ev {
                a.states.push(s);
            }
        }
        if a.states.last().is_some_and(CallState::is_final) {
            break;
        }
        assert!(Instant::now() < deadline, "states {:?}", a.states);
        std::thread::sleep(Duration::from_millis(20));
    }
    eprintln!("states after peer loss: {:?}", a.states);
    assert!(
        a.states.contains(&CallState::Reconnecting),
        "{:?}",
        a.states
    );
    assert!(
        matches!(a.states.last(), Some(CallState::Failed(_))),
        "{:?}",
        a.states
    );
}
