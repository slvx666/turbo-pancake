//! Runs `AudioEngine` on the real default devices: outgoing frames are looped back as a
//! remote peer at volume 0 (the speaker plays silence, no acoustic loop). Halfway through
//! the microphone and the speaker are reselected by id to exercise a live device switch.
//! After `stop` it checks that the devices can be opened again (they were released).
//!
//! `cargo run --release -p turbo-pancake-media-audio --example engine_check [-- <secs>]`
//! Prints one JSON object.

use std::time::{Duration, Instant};

use turbo_pancake_media_audio::device::list_devices;
use turbo_pancake_media_audio::{AudioEngine, CpalIo, EngineConfig, IncomingPacket};

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(6);
    let devices = list_devices();
    let started = Instant::now();
    let (engine, link) = AudioEngine::start(CpalIo, EngineConfig::default()).expect("start");
    let start_ms = started.elapsed().as_secs_f64() * 1000.0;
    engine.set_peer_volume(1, 0.0).unwrap();
    let looper = std::thread::spawn(move || {
        let mut n = 0u64;
        while let Ok(f) = link.outgoing.recv() {
            link.incoming.push(IncomingPacket {
                peer: 1,
                seq: f.seq,
                timestamp: f.timestamp,
                payload: f.payload,
                arrival: Instant::now(),
            });
            n += 1;
        }
        n
    });
    std::thread::sleep(Duration::from_secs(secs / 2));
    let input_id = devices
        .inputs
        .iter()
        .find(|d| d.is_default)
        .map(|d| d.id.clone());
    let output_id = devices
        .outputs
        .iter()
        .find(|d| d.is_default)
        .map(|d| d.id.clone());
    engine.set_input_device(input_id).unwrap();
    engine.set_output_device(output_id).unwrap();
    std::thread::sleep(Duration::from_secs(secs - secs / 2));
    let st = engine.status();
    let stop_at = Instant::now();
    engine.stop();
    let stop_ms = stop_at.elapsed().as_secs_f64() * 1000.0;
    let looped = looper.join().unwrap();
    // Released devices can be reopened immediately.
    let reopen_ok = AudioEngine::start(CpalIo, EngineConfig::default())
        .map(|(e, _)| {
            let ok = e.status().input.is_some() && e.status().output.is_some();
            e.stop();
            ok
        })
        .unwrap_or(false);
    let peer = st.peers.first().map(|p| p.stats).unwrap_or_default();
    let dev = |d: &Option<turbo_pancake_media_audio::engine::ActiveDevice>| {
        d.as_ref().map_or("null".to_owned(), |d| {
            format!(
                "{{\"name\":{:?},\"rate\":{},\"fallback\":{}}}",
                d.name, d.sample_rate, d.fallback
            )
        })
    };
    println!(
        "{{\"seconds\":{secs},\"start_ms\":{start_ms:.1},\"stop_ms\":{stop_ms:.1},\
\"input\":{},\"output\":{},\"frames_sent\":{},\"frames_looped\":{looped},\
\"frames_dropped\":{},\"packets_dropped\":{},\"ticks\":{},\"reopens\":{},\
\"input_xruns\":{},\"output_xruns\":{},\"backend_xruns\":{},\"peer_played\":{},\"peer_fec\":{},\
\"peer_concealed\":{},\"peer_underruns\":{},\"peer_resets\":{},\"decode_errors\":{},\
\"target_delay_ms\":{},\"dsp_available\":{},\"dsp\":\"{:?}\",\"dsp_capture_frames\":{},\"dsp_render_frames\":{},\"last_error\":{:?},\"reopen_after_stop\":{reopen_ok}}}",
        dev(&st.input),
        dev(&st.output),
        st.frames_sent,
        st.frames_dropped,
        st.packets_dropped,
        st.ticks,
        st.reopens,
        st.input_xruns,
        st.output_xruns,
        st.backend_xruns,
        peer.jitter.played,
        peer.jitter.fec,
        peer.jitter.concealed,
        peer.jitter.underruns,
        peer.jitter.resets,
        peer.decode_errors,
        peer.jitter.target_delay_ms,
        st.dsp_available,
        st.dsp,
        st.dsp_stats.capture_frames,
        st.dsp_stats.render_frames,
        st.last_error.unwrap_or_default(),
    );
}
