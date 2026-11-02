//! `AudioEngine` lifecycle on simulated devices: start/stop releases every device and
//! thread, device switch keeps the outgoing stream continuous, a failed device is
//! reopened (falling back to the default), and audio flows mic → Opus → jitter buffer →
//! mix → speaker when the outgoing frames are looped back as a remote peer.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use turbo_pancake_media_audio::sim::{SimDevice, SimIo, SimState};
use turbo_pancake_media_audio::testsignal::{rms, speech_like};
use turbo_pancake_media_audio::{
    AudioEngine, DspConfig, EncoderConfig, EngineConfig, EngineStatus, FRAME_SAMPLES,
    IncomingPacket, OutgoingFrame, VoiceDecoder, VoiceLink, VoiceSender,
};

const PEER: u64 = 7;
const DSP_OFF: DspConfig = DspConfig {
    noise_suppression: false,
    echo_cancellation: false,
    auto_gain: false,
};

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
                timestamp: f.timestamp,
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
    // The simulated microphone plays a stationary tone, which noise suppression rightly
    // removes as noise; this test checks the transport path only.
    let config = EngineConfig {
        dsp: DSP_OFF,
        ..EngineConfig::default()
    };
    let (engine, link) = AudioEngine::start(io, config).unwrap();
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
fn voice_activation_pauses_sending_and_drives_the_indicators() {
    let (io, state) = sim();
    let config = EngineConfig {
        dsp: DSP_OFF,
        voice_gate: Some(-40.0),
        ..EngineConfig::default()
    };
    let (engine, link) = AudioEngine::start(io, config).unwrap();
    let looper = loop_back(link);

    // Tone above the threshold: sent, and both indicators (own and peer) light up.
    wait_for("speaking", &engine, |s| {
        s.speaking
            && s.peers.len() == 1
            && s.peers[0].stats.speaking
            && s.peers[0].stats.jitter.played > 25
    });
    assert_eq!(engine.status().voice_gate, Some(-40.0));

    // Silence: after the hangover nothing is sent and the peer stops "speaking".
    state.muted_inputs.store(true, Ordering::Relaxed);
    wait_for("gated", &engine, |s| {
        !s.speaking && s.frames_gated > 25 && s.peers.iter().all(|p| !p.stats.speaking)
    });
    let sent = engine.status().frames_sent;
    std::thread::sleep(Duration::from_millis(300));
    let st = engine.status();
    assert_eq!(st.frames_sent, sent, "nothing sent in silence");
    assert!(st.mic_db < -90.0, "{st:#?}");

    // Speech again: the next talkspurt plays, nothing is discarded as late.
    let played = st.peers[0].stats.jitter.played;
    state.muted_inputs.store(false, Ordering::Relaxed);
    wait_for("speaking again", &engine, |s| {
        s.speaking && s.peers[0].stats.speaking && s.peers[0].stats.jitter.played > played + 25
    });
    let j = engine.status().peers[0].stats.jitter;
    assert_eq!(j.late, 0, "{j:?}");
    assert!(j.talkspurts >= 1, "{j:?}");
    assert!(j.target_delay_ms <= 100, "the pause is not jitter: {j:?}");

    // Voice activation off: silence is sent again (continuous stream).
    engine.set_voice_gate(None).unwrap();
    state.muted_inputs.store(true, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(500));
    let st = engine.status();
    assert_eq!(st.voice_gate, None);
    assert!(!st.speaking);
    let sent = st.frames_sent;
    std::thread::sleep(Duration::from_millis(300));
    assert!(engine.status().frames_sent >= sent + 10);

    engine.stop();
    looper.join().unwrap();
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

/// Speaker → microphone coupling (30 ms, −6 dB, silent near end): a remote talker is
/// played, and the returned decoded outgoing stream shows how much of them leaks back.
/// Returns outgoing level relative to the remote speech over the last 3 s, dB.
fn echo_leak_db(dsp: DspConfig) -> f32 {
    let io = SimIo::new(
        vec![SimDevice::new("mic", 48_000)],
        vec![SimDevice::new("spk", 48_000)],
    );
    let state = io.state();
    state.muted_inputs.store(true, Ordering::Relaxed);
    state.set_acoustic_echo(Some((0.5, 30, 48_000)));
    let config = EngineConfig {
        dsp,
        ..EngineConfig::default()
    };
    let (engine, link) = AudioEngine::start(io, config).unwrap();
    let secs = 10.0;
    let speech = speech_like(48_000, secs, 5);
    let incoming = link.incoming.clone();
    let remote_speech = speech.clone();
    let remote = std::thread::spawn(move || {
        let mut tx = VoiceSender::new(48_000, EncoderConfig::default()).unwrap();
        let start = Instant::now();
        for (i, chunk) in remote_speech.chunks(FRAME_SAMPLES).enumerate() {
            let due = start + Duration::from_millis(20 * i as u64);
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            tx.push_samples(chunk, |f| {
                incoming.push(IncomingPacket {
                    peer: PEER,
                    seq: f.seq,
                    timestamp: f.timestamp,
                    payload: f.payload.to_vec(),
                    arrival: Instant::now(),
                });
            })
            .unwrap();
        }
    });
    let collector = std::thread::spawn(move || {
        let mut dec = VoiceDecoder::new().unwrap();
        let mut pcm = Vec::new();
        let mut frame = vec![0.0; FRAME_SAMPLES];
        while let Ok(f) = link.outgoing.recv() {
            let n = dec.decode(&f.payload, &mut frame).unwrap();
            pcm.extend_from_slice(&frame[..n]);
        }
        pcm
    });
    remote.join().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let st = engine.status();
    engine.stop();
    let out = collector.join().unwrap();
    let tail = 48_000 * 3;
    assert!(out.len() > tail, "only {} samples sent", out.len());
    eprintln!("dsp {dsp:?}: {:?}", st.dsp_stats);
    let leak = rms(&out[out.len() - tail..]) / rms(&speech[speech.len() - tail..]);
    20.0 * leak.log10()
}

#[test]
fn acoustic_echo_loops_back_without_dsp() {
    let leak = echo_leak_db(DSP_OFF);
    eprintln!("echo leak without DSP: {leak:.1} dB");
    // The simulated room returns the remote voice at about −6 dB.
    assert!(leak > -12.0, "{leak:.1} dB");
}

/// Spec S1 "no audio loop": with AEC the remote voice does not come back. Measured
/// −57…−68 dB on Windows/Linux; the macOS CI VM reached −24.6 dB once (simulated device
/// threads jitter there, so the echo delay is not constant), hence the −20 dB bound.
#[cfg(feature = "apm")]
#[test]
fn echo_canceller_breaks_the_audio_loop() {
    let leak = echo_leak_db(DspConfig::default());
    eprintln!("echo leak with AEC3 + NS: {leak:.1} dB");
    assert!(leak < -20.0, "{leak:.1} dB");
}
