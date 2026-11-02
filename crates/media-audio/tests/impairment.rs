//! Voice continuity under simulated network impairment (spec §13 network profiles).
//!
//! Sender: speech-like signal at 192 kHz (the dev machine's microphone rate) fed in 10 ms
//! device-sized chunks → `VoiceSender`. Network: deterministic random loss, base delay
//! plus uniform jitter (which also reorders), optional outage. Receiver: `VoiceReceiver`
//! ticked every 20 ms on its own clock, rendering at 44.1 kHz.
//!
//! Set `TURBO_PANCAKE_REPORT_DIR` to also write the measured table as JSON.

use std::sync::OnceLock;
use turbo_pancake_media_audio::testsignal::{Rng, rms, speech_like};

use turbo_pancake_media_audio::{
    EncoderConfig, FRAME_MS, JitterConfig, MonoResampler, ReceiverStats, VoiceReceiver, VoiceSender,
};

const MIC_RATE: u32 = 192_000;
const OUT_RATE: u32 = 44_100;
const SECS: f32 = 20.0;

/// The same 20 s "microphone" signal for every scenario: generated at 48 kHz and
/// upsampled once to the device rate (generating at 192 kHz directly is slow in debug).
fn mic_signal() -> &'static [f32] {
    static SIGNAL: OnceLock<Vec<f32>> = OnceLock::new();
    SIGNAL.get_or_init(|| {
        let base = speech_like(48_000, SECS, 42);
        let mut up = Vec::with_capacity(base.len() * 4);
        let mut r = MonoResampler::new(48_000, MIC_RATE).unwrap();
        r.process(&base, &mut up).unwrap();
        up
    })
}

#[derive(Debug, Clone, Copy)]
struct Profile {
    loss: f32,
    jitter_ms: u64,
    base_delay_ms: u64,
    /// (start_ms, len_ms) during which everything is dropped.
    outage: Option<(u64, u64)>,
}

struct Outcome {
    stats: ReceiverStats,
    network_lost: u64,
    sent: u64,
    level_ratio: f32,
    max_target_ms: u32,
}

fn run(profile: Profile, seed: u64) -> Outcome {
    let input = mic_signal();
    let mut sender = VoiceSender::new(MIC_RATE, EncoderConfig::default()).unwrap();
    let mut net = Rng::new(seed ^ 0x9e37_79b9);
    // (arrival_ms, seq, payload)
    let mut in_flight: Vec<(u64, u16, Vec<u8>)> = Vec::new();
    let mut lost = 0;
    let mut sent = 0u64;
    for chunk in input.chunks(MIC_RATE as usize / 100) {
        sender
            .push_samples(chunk, |f| {
                let send_ms = u64::from(f.timestamp / 48);
                sent += 1;
                let in_outage = profile
                    .outage
                    .is_some_and(|(s, l)| send_ms >= s && send_ms < s + l);
                if in_outage || net.unit() < profile.loss {
                    lost += 1;
                    return;
                }
                let jitter = if profile.jitter_ms > 0 {
                    net.next_u64() % (profile.jitter_ms + 1)
                } else {
                    0
                };
                in_flight.push((
                    send_ms + profile.base_delay_ms + jitter,
                    f.seq,
                    f.payload.to_vec(),
                ));
            })
            .unwrap();
    }
    in_flight.sort_by_key(|p| p.0);

    let mut rx = VoiceReceiver::new(OUT_RATE, JitterConfig::default()).unwrap();
    let mut out = Vec::new();
    let mut next = 0;
    let mut max_target = 0;
    // Receiver clock is offset by 7 ms from the sender's frame grid. Run until every
    // packet was delivered and played out (end of stream is not an underrun).
    let mut t = 7;
    while next < in_flight.len() || rx.buffered_frames() > 0 {
        while next < in_flight.len() && in_flight[next].0 <= t {
            let (arrival, seq, payload) = std::mem::take(&mut in_flight[next]);
            rx.push_packet(seq, payload, arrival);
            next += 1;
        }
        rx.pull_frame(&mut out).unwrap();
        max_target = max_target.max(rx.stats().jitter.target_delay_ms);
        t += u64::from(FRAME_MS);
    }
    // Compare loudness after the first second (startup buffering excluded).
    let skip = OUT_RATE as usize;
    let level_ratio = rms(&out[skip..]) / rms(&input[MIC_RATE as usize..]);
    Outcome {
        stats: rx.stats(),
        network_lost: lost,
        sent,
        level_ratio,
        max_target_ms: max_target,
    }
}

fn row(name: &str, p: Profile, o: &Outcome) -> String {
    let j = o.stats.jitter;
    format!(
        "{{\"profile\":\"{name}\",\"loss_pct\":{:.0},\"jitter_ms\":{},\"sent\":{},\"net_lost\":{},\
\"played\":{},\"fec\":{},\"concealed\":{},\"underruns\":{},\"late\":{},\"dropped\":{},\
\"repaired_pct\":{:.2},\"concealed_pct\":{:.2},\"final_target_ms\":{},\"max_target_ms\":{},\
\"level_ratio\":{:.3},\"decode_errors\":{}}}",
        p.loss * 100.0,
        p.jitter_ms,
        o.sent,
        o.network_lost,
        j.played,
        j.fec,
        j.concealed,
        j.underruns,
        j.late,
        j.dropped,
        o.stats.repaired_ratio() * 100.0,
        o.stats.concealed_ratio() * 100.0,
        j.target_delay_ms,
        o.max_target_ms,
        o.level_ratio,
        o.stats.decode_errors,
    )
}

#[test]
fn continuity_matrix() {
    let mut rows = Vec::new();
    for &loss in &[0.0f32, 0.01, 0.03, 0.05, 0.10] {
        for &jitter_ms in &[0u64, 20, 50] {
            let p = Profile {
                loss,
                jitter_ms,
                base_delay_ms: 30,
                outage: None,
            };
            let o = run(p, 42 + (loss * 100.0) as u64 * 3 + jitter_ms);
            let name = format!("loss{}_jitter{}", (loss * 100.0) as u32, jitter_ms);
            let line = row(&name, p, &o);
            println!("{line}");
            rows.push(line);

            let j = o.stats.jitter;
            assert_eq!(o.stats.decode_errors, 0, "{name}");
            // Ticks with nothing buffered ahead (the newest packets lost or late) stay
            // rare; they are concealed, the playout clock never stops.
            assert!(
                j.underruns * 500 <= o.sent,
                "{name}: {} underruns",
                j.underruns
            );
            // Late arrivals are rare: the adaptive delay covers the jitter.
            assert!(j.late * 100 <= o.sent, "{name}: {} late", j.late);
            // Loss is repaired by FEC first; concealment never exceeds the loss itself
            // (plus late/latency drops).
            if loss > 0.0 {
                assert!(j.fec > 0, "{name}: FEC never used");
            }
            assert!(
                j.concealed <= o.network_lost + j.late + j.dropped,
                "{name}: concealed {} > lost {}",
                j.concealed,
                o.network_lost
            );
            // Loudness is preserved (no muting/gaps), within codec tolerance.
            assert!(
                (0.75..1.25).contains(&o.level_ratio),
                "{name}: level ratio {}",
                o.level_ratio
            );
            // Delay adapts to jitter but stays bounded.
            assert!(
                j.target_delay_ms <= jitter_ms as u32 + 60,
                "{name}: target {} ms",
                j.target_delay_ms
            );
        }
    }
    if let Ok(dir) = std::env::var("TURBO_PANCAKE_REPORT_DIR") {
        let path = std::path::Path::new(&dir).join("audio-impairment.json");
        std::fs::write(&path, format!("[\n{}\n]\n", rows.join(",\n"))).unwrap();
        println!("report: {}", path.display());
    }
}

#[test]
fn recovers_after_outage() {
    let p = Profile {
        loss: 0.01,
        jitter_ms: 20,
        base_delay_ms: 30,
        outage: Some((10_000, 2_000)),
    };
    let o = run(p, 7);
    println!("{}", row("outage_2s", p, &o));
    let j = o.stats.jitter;
    // The outage shows up as concealment/rebuffering, not as an error or a stuck stream.
    assert!(j.underruns > 0);
    assert_eq!(o.stats.decode_errors, 0);
    // After the outage playback resumes: almost every packet sent outside it is played.
    let outside = o.sent - 100; // 2 s of 20 ms frames were dropped by the outage
    assert!(
        j.played + j.fec >= outside * 95 / 100,
        "played {} of {outside}",
        j.played
    );
    assert!(j.target_delay_ms <= 80, "target {} ms", j.target_delay_ms);
}
