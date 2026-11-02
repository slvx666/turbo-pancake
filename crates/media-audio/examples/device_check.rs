//! Drives the real default microphone and speaker through the whole voice pipeline for a
//! few seconds: mic → resample → Opus → jitter buffer → decode → resample → speaker.
//! The receiver volume is 0, so the speaker only plays silence (no acoustic loop, no
//! noise for whoever sits at the machine), but every stage runs on real devices.
//!
//! `cargo run --release -p turbo-pancake-media-audio --example device_check [-- <secs>]`
//! Prints one JSON object.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use turbo_pancake_media_audio::device::{list_devices, open_capture, open_playback};
use turbo_pancake_media_audio::{EncoderConfig, JitterConfig, VoiceReceiver, VoiceSender};

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let devices = list_devices();
    let (capture, mut mic) = open_capture(None, 200).expect("open microphone");
    let (playback, mut speaker) = open_playback(None, 200).expect("open speaker");

    let mut sender = VoiceSender::new(capture.sample_rate, EncoderConfig::default()).unwrap();
    let mut receiver = VoiceReceiver::new(playback.sample_rate, JitterConfig::default()).unwrap();
    receiver.set_volume(0.0);

    let start = Instant::now();
    let mut buf = Vec::with_capacity(capture.sample_rate as usize / 10);
    let mut out = Vec::with_capacity(playback.sample_rate as usize / 10);
    let mut mic_samples = 0u64;
    let mut peak = 0.0f32;
    let mut bytes = 0usize;
    let mut pulled = 0u64;
    // Keep ~40 ms queued for the speaker: the output device clock paces decoding.
    let speaker_target = (playback.sample_rate / 25) as usize;
    while start.elapsed() < Duration::from_secs(secs) {
        buf.clear();
        while let Ok(s) = mic.pop() {
            buf.push(s);
        }
        mic_samples += buf.len() as u64;
        let now_ms = start.elapsed().as_millis() as u64;
        let mut packets = Vec::new();
        sender
            .push_samples(&buf, |f| {
                peak = peak.max(f.peak);
                bytes += f.payload.len();
                packets.push((f.seq, f.payload.to_vec()));
            })
            .unwrap();
        for (seq, p) in packets {
            receiver.push_packet(seq, p, now_ms);
        }
        let queued = speaker.buffer().capacity() - speaker.slots();
        if queued < speaker_target {
            out.clear();
            receiver.pull_frame(&mut out).unwrap();
            pulled += 1;
            for &s in &out {
                if speaker.push(s).is_err() {
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(5));
        if capture.health.failed() || playback.health.failed() {
            eprintln!("device failed");
            break;
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let rx = receiver.stats();
    println!(
        "{{\"inputs\":{},\"outputs\":{},\"input\":{{\"name\":{:?},\"rate\":{},\"channels\":{},\
\"samples_per_sec\":{:.0},\"callbacks\":{},\"overflow_samples\":{}}},\
\"output\":{{\"name\":{:?},\"rate\":{},\"channels\":{},\"callbacks\":{},\"underrun_samples\":{}}},\
\"frames_sent\":{},\"kbps\":{:.1},\"mic_peak\":{:.3},\"ticks_pulled\":{},\"played\":{},\
\"concealed\":{},\"target_delay_ms\":{},\"seconds\":{:.2},\"failed\":{}}}",
        devices.inputs.len(),
        devices.outputs.len(),
        capture.device_name,
        capture.sample_rate,
        capture.channels,
        mic_samples as f64 / elapsed,
        capture.health.callbacks.load(Ordering::Relaxed),
        capture.health.xruns.load(Ordering::Relaxed),
        playback.device_name,
        playback.sample_rate,
        playback.channels,
        playback.health.callbacks.load(Ordering::Relaxed),
        playback.health.xruns.load(Ordering::Relaxed),
        sender.frames_sent(),
        bytes as f64 * 8.0 / elapsed / 1000.0,
        peak,
        pulled,
        rx.jitter.played,
        rx.jitter.concealed,
        rx.jitter.target_delay_ms,
        elapsed,
        capture.health.failed() || playback.health.failed(),
    );
}
