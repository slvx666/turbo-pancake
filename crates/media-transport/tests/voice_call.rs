//! Two headless clients in one process: each is a real `AudioEngine` (simulated devices)
//! plus a `CallDriver`, talking over real loopback UDP with ICE + DTLS-SRTP, with test
//! loss on the wire (spec S1 plan item 5). Proves the engine ↔ transport wiring the
//! desktop client will use: mic → Opus → RTP → jitter buffer → speaker, continuity under
//! loss, and that leaving releases devices, threads and the socket.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use turbo_pancake_media_audio::sim::{SimDevice, SimIo, SimState};
use turbo_pancake_media_audio::{
    AudioEngine, DspConfig, EngineConfig, EngineStatus, IncomingPacket,
};
use turbo_pancake_media_transport::call::{
    AudioOut, CallConfig, CallDriver, CallEvent, CallState, LocalSignal, Role,
};

/// Peer id the engine uses for the remote side of a 1:1 call.
const REMOTE: u64 = 1;

struct Client {
    engine: AudioEngine,
    driver: CallDriver,
    events: Receiver<CallEvent>,
    pump: JoinHandle<()>,
    state: Arc<SimState>,
    states: Vec<CallState>,
}

fn client(role: Role, mic_on: bool, send_loss: f32) -> Client {
    let io = SimIo::new(
        vec![SimDevice::new("mic", 48_000)],
        vec![SimDevice::new("spk", 48_000)],
    );
    let state = io.state();
    state.muted_inputs.store(!mic_on, Ordering::Relaxed);
    // The simulated mic is a stationary tone, which noise suppression removes as noise.
    let config = EngineConfig {
        dsp: DspConfig {
            noise_suppression: false,
            echo_cancellation: false,
            auto_gain: false,
        },
        ..EngineConfig::default()
    };
    let (engine, link) = AudioEngine::start(io, config).unwrap();
    // Engine frames → call. A full call queue drops (the call thread is behind).
    let (to_call, from_engine) = sync_channel::<AudioOut>(50);
    let outgoing = link.outgoing;
    let pump = std::thread::spawn(move || {
        while let Ok(f) = outgoing.recv() {
            let _ = to_call.try_send(AudioOut {
                seq: f.seq,
                timestamp: f.timestamp,
                payload: f.payload,
            });
        }
    });
    let sink = link.incoming;
    let mut cfg = CallConfig::new(role, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    cfg.send_loss = send_loss;
    let (driver, events) = CallDriver::start(cfg, from_engine, move |p| {
        sink.push(IncomingPacket {
            peer: REMOTE,
            seq: p.seq,
            timestamp: p.timestamp,
            payload: p.payload,
            arrival: p.arrival,
        });
    })
    .unwrap();
    Client {
        engine,
        driver,
        events,
        pump,
        state,
        states: Vec::new(),
    }
}

fn relay(from: &mut Client, to: &Client) {
    while let Ok(ev) = from.events.try_recv() {
        match ev {
            CallEvent::Signal(LocalSignal::Offer(s)) => to.driver.remote_offer(s).unwrap(),
            CallEvent::Signal(LocalSignal::Answer(s)) => to.driver.remote_answer(s).unwrap(),
            CallEvent::Signal(LocalSignal::Candidate(c)) => to.driver.remote_candidate(c).unwrap(),
            CallEvent::State(s) => from.states.push(s),
            CallEvent::KeyframeRequested => {}
        }
    }
}

fn connected(c: &Client) -> bool {
    c.states.last() == Some(&CallState::Connected)
}

fn wait(
    what: &str,
    secs: u64,
    mut a: Option<&mut Client>,
    mut b: Option<&mut Client>,
    done: impl Fn(&EngineStatus) -> bool,
    eng: &AudioEngine,
) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !done(&eng.status()) {
        if let (Some(a), Some(b)) = (a.as_deref_mut(), b.as_deref_mut()) {
            relay(a, b);
            relay(b, a);
        }
        assert!(
            Instant::now() < deadline,
            "timed out: {what}: {:#?}",
            eng.status()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn hang_up(c: Client) {
    let Client {
        engine,
        driver,
        pump,
        state,
        ..
    } = c;
    driver.stop();
    engine.stop();
    pump.join().unwrap();
    assert_eq!(state.open_inputs.load(Ordering::Relaxed), 0);
    assert_eq!(state.open_outputs.load(Ordering::Relaxed), 0);
    assert_eq!(
        state.threads.load(Ordering::Relaxed),
        0,
        "device threads left"
    );
}

#[test]
fn two_clients_talk_over_udp_with_loss() {
    // A talks (440 Hz, peak 0.3) and loses 5 % of what it sends; B listens.
    let mut a = client(Role::Offerer, true, 0.05);
    let mut b = client(Role::Answerer, false, 0.0);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(connected(&a) && connected(&b)) {
        relay(&mut a, &b);
        relay(&mut b, &a);
        assert!(
            Instant::now() < deadline,
            "a {:?} b {:?}",
            a.states,
            b.states
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let connected_at = Instant::now();
    // 6 s of conversation measured at B.
    let b_engine = &b.engine;
    wait(
        "6 s of audio at b",
        20,
        Some(&mut a),
        None,
        |s| {
            s.peers
                .first()
                .is_some_and(|p| p.stats.jitter.played >= 300)
        },
        b_engine,
    );
    let st = b.engine.status();
    let rx = st.peers[0].stats;
    let call = a.driver.stats();
    eprintln!(
        "b receiver after {:?}: {rx:?}\na call: {call:?}\nb speaker recent peak {:.3}",
        connected_at.elapsed(),
        b.state.recent_peak()
    );
    assert_eq!(rx.decode_errors, 0);
    assert!(rx.jitter.late <= 3, "late {}", rx.jitter.late);
    // 5 % random loss: FEC + PLC repair it; concealment-only stays small.
    assert!(rx.repaired_ratio() > 0.01, "loss not visible? {rx:?}");
    assert!(rx.concealed_ratio() < 0.03, "{rx:?}");
    assert!(
        b.state.recent_peak() > 0.2,
        "speaker peak {}",
        b.state.recent_peak()
    );
    assert!(call.impaired_drops > 0 && call.packets_sent >= 300);
    // B's silent mic produces silence at A.
    assert!(
        a.state.recent_peak() < 0.01,
        "a speaker {}",
        a.state.recent_peak()
    );

    // A leaves: B notices (reconnecting), both release everything.
    hang_up(a);
    let deadline = Instant::now() + Duration::from_secs(20);
    while !b.states.contains(&CallState::Reconnecting) {
        while let Ok(ev) = b.events.try_recv() {
            if let CallEvent::State(s) = ev {
                b.states.push(s);
            }
        }
        assert!(Instant::now() < deadline, "b states {:?}", b.states);
        std::thread::sleep(Duration::from_millis(20));
    }
    hang_up(b);
}
