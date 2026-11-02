//! CPU cost of the voice pipeline stages on this machine (20 s speech-like signal).
//!
//! `cargo run --release -p turbo-pancake-media-audio --example codec_cost`
//! Prints one JSON object; per-frame times are wall-clock averages on one thread.

use std::time::Instant;

use turbo_pancake_media_audio::testsignal::speech_like;
use turbo_pancake_media_audio::{
    EncoderConfig, FRAME_SAMPLES, MAX_PACKET, MonoResampler, VoiceDecoder, VoiceEncoder,
};

fn main() {
    let secs = 20.0;
    let signal = speech_like(48_000, secs, 1);
    let frames = signal.len() / FRAME_SAMPLES;
    let mut encode = Vec::new();
    let mut packets = Vec::new();
    for complexity in [0, 5, 9, 10] {
        let config = EncoderConfig {
            complexity,
            ..EncoderConfig::default()
        };
        let mut enc = VoiceEncoder::new(config).unwrap();
        let mut buf = [0u8; MAX_PACKET];
        let mut bytes = 0;
        let start = Instant::now();
        for f in signal.as_chunks::<FRAME_SAMPLES>().0 {
            let n = enc.encode(f, &mut buf).unwrap();
            bytes += n;
            if complexity == EncoderConfig::default().complexity {
                packets.push(buf[..n].to_vec());
            }
        }
        let us = start.elapsed().as_secs_f64() * 1e6 / frames as f64;
        encode.push(format!(
            "{{\"complexity\":{complexity},\"us_per_frame\":{us:.1},\"kbps\":{:.1}}}",
            bytes as f64 * 8.0 / f64::from(secs) / 1000.0
        ));
    }
    let mut dec = VoiceDecoder::new().unwrap();
    let mut pcm = vec![0.0; FRAME_SAMPLES];
    let start = Instant::now();
    for p in &packets {
        dec.decode(p, &mut pcm).unwrap();
    }
    let decode_us = start.elapsed().as_secs_f64() * 1e6 / packets.len() as f64;

    let mic: Vec<f32> = signal.iter().flat_map(|v| [*v; 4]).collect();
    let mut down = MonoResampler::new(192_000, 48_000).unwrap();
    let mut out = Vec::with_capacity(signal.len() + 4096);
    let start = Instant::now();
    for chunk in mic.chunks(1920) {
        down.process(chunk, &mut out).unwrap();
    }
    let down_us = start.elapsed().as_secs_f64() * 1e6 / frames as f64;

    println!(
        "{{\"frames\":{frames},\"encode\":[{}],\"decode_us_per_frame\":{decode_us:.1},\
\"resample_192k_to_48k_us_per_frame\":{down_us:.1}}}",
        encode.join(",")
    );
}
