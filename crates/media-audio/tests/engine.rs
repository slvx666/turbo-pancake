//! `AudioEngine` lifecycle on simulated devices: start/stop releases every device and
//! thread, device switch keeps the outgoing stream continuous, a failed device is
//! reopened (falling back to the default), and audio flows mic → Opus → jitter buffer →
//! mix → speaker when the outgoing frames are looped back as a remote peer.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use turbo_pancake_media_audio::sim::{SimDevice, SimIo, SimState};
use turbo_pancake_media_audio::{
    AudioEngine, EngineConfig, EngineStatus, IncomingPacket, OutgoingFrame, VoiceLink,
};

const PEER: u64 = 7;

fn sim() -> (SimIo, Arc<SimState>) {
    let io = SimIo::new(
        vec![
            SimDevice::new("mic-48k", 48_000),
            SimDevice::new("mic-44k", 44_100),
        ],
        vec![
            SimDevice::new("spk-48k", 48_000),
            SimDevice::new("spk-96k", 96_000),
        ],
    );
    let state = io.state();
    (io, state)
}

/// Polls `cond` until it holds (CI runners are slow and shared: generous timeout).
fn wait_for(what: &str, engine: &AudioEngine, cond: impl Fn(&EngineStatus) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let st = engine.status();
        if cond(&st) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {st:#?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Feeds every outgoing frame back into the engine as peer `PEER` and records it.
fn loop_back(link: VoiceLink) -> JoinHandle<Vec<OutgoingFrame>> {
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        while let Ok(f) = link.outgoing.recv() {
            link.incoming.push(IncomingPacket {
                peer: PEER,
                seq: f.seq,
                payload: f.payload.clone(),
                arrival: Instant::now(),
            });
            seen.push(f);
        }
        seen
    })
}

#[test]
fn start_stop_releases_devices_and_threads() {
    let (io, state) = sim();
    let (engine, link) = AudioEngine::start(io, EngineConfig::default()).unwrap();
    let st = engine.status();
    assert!(st.running);
    assert_eq!(st.input.as_ref().unwrap().name, "mic-48k");
    assert_eq!(st.output.as_ref().unwrap().name, "spk-48k");
    assert_eq!(state.open_inputs.load(Ordering::Relaxed), 1);
    assert_eq!(state.open_outputs.load(Ordering::Relaxed), 1);
    wait_for("frames", &engine, |s| s.frames_sent >= 10 && s.ticks >= 10);

    engine.stop();
    assert_eq!(state.open_inputs.load(Ordering::Relaxed), 0);
    assert_eq!(state.open_outputs.load(Ordering::Relaxed), 0);
    assert_eq!(
        state.threads.load(Ordering::Relaxed),
        0,
        "device threads left"
    );
    // The engine thread has exited: its end of the outgoing channel is closed.
    while link.outgoing.try_recv().is_ok() {}
    assert!(matches!(
        link.outgoing.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Disconnected)
    ));
    // Dropping the handle without `stop` releases everything as well.
    let (io, state) = sim();
    let (engine, _link) = AudioEngine::start(io, EngineConfig::default()).unwrap();
    drop(engine);
    assert_eq!(state.open_inputs.load(Ordering::Relaxed), 0);
    assert_eq!(state.threads.load(Ordering::Relaxed), 0);
}

#[test]
fn loopback_audio_reaches_the_speaker_with_volume_and_mute() {
    let (io, state) = sim();
    let (engine, link) = AudioEngine::start(io, EngineConfig::default()).unwrap();
    let looper = loop_back(link);

    // Tone peak 0.3 → Opus → jitter buffer → mix → speaker.
    wait_for("peer audio", &engine, |s| {
        s.peers.len() == 1 && s.peers[0].stats.jitter.played > 25
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while state.recent_peak() < 0.2 {
        assert!(
            Instant::now() < deadline,
            "speaker peak {}",
            state.recent_peak()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let st = engine.status();
    assert!(st.mic_peak > 0.25, "{st:#?}");
    assert_eq!(st.peers[0].stats.decode_errors, 0);

    engine.set_peer_volume(PEER, 0.0).unwrap();
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        state.recent_peak() < 0.01,
        "volume 0: {}",
        state.recent_peak()
    );

    engine.set_peer_volume(PEER, 1.0).unwrap();
    engine.set_muted(true).unwrap();
    std::thread::sleep(Duration::from_millis(400));
    let st = engine.status();
    assert!(st.muted);
    assert_eq!(st.mic_peak, 0.0);
    assert!(state.recent_peak() < 0.01, "muted: {}", state.recent_peak());

    engine.stop();
    let frames = looper.join().unwrap();
    assert!(frames.len() > 40);
    // Muted frames are still sent (silence keeps the receiver clock running).
    assert!(frames.iter().rev().take(5).all(|f| f.peak == 0.0));
}

#[test]
fn device_switch_keeps_the_stream_continuous() {
    let (io, state) = sim();
    let (engine, link) = AudioEngine::start(io, EngineConfig::default()).unwrap();
    let looper = loop_back(link);
    wait_for("frames", &engine, |s| s.frames_sent >= 10);

    engine.set_input_device(Some("mic-44k".into())).unwrap();
    engine.set_output_device(Some("spk-96k".into())).unwrap();
    wait_for("switch", &engine, |s| {
        s.input.as_ref().is_some_and(|d| d.sample_rate == 44_100)
            && s.output.as_ref().is_some_and(|d| d.sample_rate == 96_000)
    });
    let before = engine.status().frames_sent;
    wait_for("frames after switch", &engine, |s| {
        s.frames_sent >= before + 25
    });
    let st = engine.status();
    assert_eq!(st.reopens, 2);
    assert!(!st.input.unwrap().fallback);
    assert_eq!(state.open_inputs.load(Ordering::Relaxed), 1);
    assert_eq!(state.open_outputs.load(Ordering::Relaxed), 1);
    assert_eq!(state.input_opens.load(Ordering::Relaxed), 2);
    assert_eq!(state.threads.load(Ordering::Relaxed), 2);

    engine.stop();
    let frames = looper.join().unwrap();
    for w in frames.windows(2) {
        assert_eq!(w[1].seq, w[0].seq.wrapping_add(1), "sequence gap");
        assert_eq!(w[1].timestamp, w[0].timestamp.wrapping_add(960));
    }
    assert_eq!(state.threads.load(Ordering::Relaxed), 0);
}

#[test]
fn failed_device_is_reopened_with_fallback_to_default() {
    let (io, state) = sim();
    let config = EngineConfig {
        input: Some("mic-44k".into()),
        ..EngineConfig::default()
    };
    let (engine, _link) = AudioEngine::start(io, config).unwrap();
    assert_eq!(engine.status().input.unwrap().name, "mic-44k");

    // Unplug the selected microphone mid-call: the engine falls back to the default.
    state.set_unavailable("mic-44k", true);
    state.fail_device("mic-44k");
    wait_for("fallback", &engine, |s| {
        s.input
            .as_ref()
            .is_some_and(|d| d.name == "mic-48k" && d.fallback)
    });
    let sent = engine.status().frames_sent;
    wait_for("frames on fallback", &engine, |s| s.frames_sent > sent + 10);

    // Speaker gone with no alternative: no output, retried every second; receivers keep
    // ticking on the wall clock meanwhile.
    state.set_unavailable("spk-48k", true);
    state.fail_device("spk-48k");
    wait_for("no output", &engine, |s| s.output.is_none());
    let ticks = engine.status().ticks;
    wait_for("wall-clock ticks", &engine, |s| s.ticks > ticks + 10);
    assert_eq!(state.open_outputs.load(Ordering::Relaxed), 0);
    assert!(engine.status().last_error.is_some());

    state.set_unavailable("spk-48k", false);
    wait_for("output back", &engine, |s| s.output.is_some());
    assert_eq!(state.open_inputs.load(Ordering::Relaxed), 1);
    assert_eq!(state.open_outputs.load(Ordering::Relaxed), 1);
    engine.stop();
    assert_eq!(state.threads.load(Ordering::Relaxed), 0);
}

#[test]
fn unknown_device_at_start_falls_back_to_default() {
    let (io, _state) = sim();
    let config = EngineConfig {
        input: Some("no-such-mic".into()),
        output: Some("no-such-speaker".into()),
        ..EngineConfig::default()
    };
    let (engine, _link) = AudioEngine::start(io, config).unwrap();
    let st = engine.status();
    assert!(st.input.as_ref().unwrap().fallback);
    assert!(st.output.as_ref().unwrap().fallback);
    assert_eq!(st.output.unwrap().name, "spk-48k");
    engine.remove_peer(1).unwrap();
    engine.stop();
}
