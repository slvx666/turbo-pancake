//! Encode → decode through the OS codecs (S3 step 3): every frame comes back, in order,
//! with the picture intact, a requested keyframe is honoured, and on a hardware encoder
//! 1080p60 in real time is measured.

#![cfg(windows)]

use std::time::{Duration, Instant};

use turbo_pancake_media_video::win::{
    EncoderInput, H264Decoder, H264Encoder, adapter_name, create_device, qpc_100ns, startup,
};
use turbo_pancake_media_video::{EncodedFrame, EncoderConfig, VideoError};

/// NV12 test picture: a diagonal gradient with a bright square moving with `n`.
fn picture(width: u32, height: u32, n: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut out = vec![128u8; w * h * 3 / 2];
    let side = h / 4;
    let x0 = (n as usize * 8) % (w - side);
    let y0 = (n as usize * 4) % (h - side);
    for y in 0..h {
        for x in 0..w {
            let inside = (x0..x0 + side).contains(&x) && (y0..y0 + side).contains(&y);
            out[y * w + x] = if inside {
                235
            } else {
                (16 + (x + y) * 160 / (w + h)) as u8
            };
        }
    }
    out
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let mse = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| (x as f64 - y as f64).powi(2))
        .sum::<f64>()
        / a.len() as f64;
    if mse == 0.0 {
        99.0
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

fn wait_ready(encoder: &mut H264Encoder) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !encoder.ready().expect("encoder events") {
        assert!(Instant::now() < deadline, "encoder never asked for input");
        std::thread::sleep(Duration::from_micros(500));
    }
}

#[test]
fn every_frame_round_trips_and_keyframe_requests_work() {
    round_trip(false);
}

/// The OS software encoder (what machines without a hardware encoder get).
#[test]
fn software_encoder_round_trips() {
    round_trip(true);
}

fn round_trip(software_only: bool) {
    startup().expect("Media Foundation");
    let (device, _context, hardware_device) = create_device().expect("D3D11 device");
    let (width, height, frames) = (640u32, 360u32, 90u32);
    let mut config = EncoderConfig::new(width, height, 30, 2000);
    config.gop_frames = 0;
    config.software_only = software_only;
    let mut encoder = match H264Encoder::new(&device, &config) {
        Ok(e) => e,
        Err(VideoError::NoCodec(what)) => {
            // Windows editions without Media Foundation (N editions, some Server images).
            println!("SKIP: no H.264 {what} on this system");
            return;
        }
        Err(e) => panic!("encoder: {e}"),
    };
    // DXVA on a WARP (software) device is pointless; CI runners have no GPU.
    let mut decoder = H264Decoder::new(hardware_device.then_some(&device)).expect("decoder");
    println!(
        "encoder {:?} hardware={} settings={:?}; decoder dxva={}; device hardware={} {:?}",
        encoder.name(),
        encoder.is_hardware(),
        encoder.settings_applied(),
        decoder.is_hardware(),
        hardware_device,
        adapter_name(&device),
    );

    let duration = 10_000_000 / 30;
    let mut encoded: Vec<EncodedFrame> = Vec::new();
    let mut decoded = Vec::new();
    let mut feed = |frame: EncodedFrame, decoder: &mut H264Decoder, encoded: &mut Vec<_>| {
        decoder
            .decode(&frame.data, frame.timestamp)
            .expect("decode");
        encoded.push(frame);
        while let Some(p) = decoder.poll_output() {
            decoded.push(p);
        }
    };
    for n in 0..frames {
        if n == 45 {
            encoder.request_keyframe().expect("keyframe request");
        }
        wait_ready(&mut encoder);
        let bytes = picture(width, height, n);
        encoder
            .encode(EncoderInput::Nv12(&bytes), n as i64 * duration, duration)
            .expect("encode");
        while let Some(frame) = encoder.poll_output().expect("encoder output") {
            feed(frame, &mut decoder, &mut encoded);
        }
    }
    for frame in encoder.drain().expect("encoder drain") {
        feed(frame, &mut decoder, &mut encoded);
    }
    decoded.extend(decoder.drain().expect("decoder drain"));

    assert_eq!(encoded.len(), frames as usize, "every frame encoded");
    assert!(encoded[0].keyframe, "stream starts with a keyframe");
    assert!(
        encoded[0].data.starts_with(&[0, 0, 0, 1]) || encoded[0].data.starts_with(&[0, 0, 1]),
        "Annex B output"
    );
    let forced = encoded
        .iter()
        .find(|f| f.timestamp == 45 * duration)
        .expect("frame 45 encoded");
    assert!(forced.keyframe, "requested keyframe at frame 45");
    let keyframes = encoded.iter().filter(|f| f.keyframe).count();
    assert!(
        keyframes <= 3,
        "no periodic keyframes with gop 0: {keyframes}"
    );

    assert_eq!(decoded.len(), frames as usize, "every frame decoded");
    let mut worst = f64::MAX;
    for (n, picture_out) in decoded.iter().enumerate() {
        assert_eq!((picture_out.width, picture_out.height), (width, height));
        assert_eq!(picture_out.timestamp, n as i64 * duration, "decode order");
        let source = picture(width, height, n as u32);
        let p = psnr(picture_out.luma(), &source[..(width * height) as usize]);
        worst = worst.min(p);
    }
    let bytes: usize = encoded.iter().map(|f| f.data.len()).sum();
    println!(
        "{frames} frames, {keyframes} keyframes, {:.0} kbit/s, worst luma PSNR {worst:.1} dB",
        bytes as f64 * 8.0 / (frames as f64 / 30.0) / 1000.0
    );
    assert!(
        worst > 30.0,
        "picture survives the round trip: {worst:.1} dB"
    );
}

fn percentile(values: &mut [f64], p: f64) -> f64 {
    values.sort_by(|a, b| a.total_cmp(b));
    values[((values.len() - 1) as f64 * p).round() as usize]
}

/// Hardware encoder only: 1080p60 paced in real time for 3 s; prints encode and
/// encode + decode latency. Skips on machines without a hardware encoder (CI).
#[test]
fn hardware_1080p60_real_time() {
    real_time(1920, 1080, 60, 8000, 180);
}

/// S4 top mode: 1440p120 paced in real time for 3 s (encoder + DXVA decoder only; the
/// pictures are uploaded from memory, the share itself hands the encoder GPU textures).
#[test]
fn hardware_1440p120_real_time() {
    real_time(2560, 1440, 120, 20_000, 360);
}

fn real_time(width: u32, height: u32, fps: u32, kbps: u32, frames: u32) {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    startup().expect("Media Foundation");
    let (device, _context, _) = create_device().expect("D3D11 device");
    let mut config = EncoderConfig::new(width, height, fps, kbps);
    config.require_hardware = true;
    let mut encoder = match H264Encoder::new(&device, &config) {
        Ok(e) => e,
        Err(e) => {
            println!("SKIP: no hardware H.264 encoder ({e})");
            return;
        }
    };
    // The decoder runs on its own thread (as on a viewer: another thread, usually another
    // machine), so encode and decode overlap like in a call.
    let submitted_at: Arc<Mutex<HashMap<i64, i64>>> = Arc::default();
    let (tx, rx) = std::sync::mpsc::channel::<EncodedFrame>();
    let at = submitted_at.clone();
    let decoder_thread = std::thread::spawn(move || {
        startup().expect("Media Foundation");
        let (device, _, _) = create_device().expect("D3D11 device");
        let use_dxva = std::env::var_os("TURBO_PANCAKE_SW_DECODE").is_none();
        let mut decoder = H264Decoder::new(use_dxva.then_some(&device)).expect("decoder");
        let (mut total_ms, mut decode_call_ms) = (Vec::new(), Vec::new());
        let mut decoded = 0u32;
        for frame in rx {
            let d0 = Instant::now();
            decoder
                .decode(&frame.data, frame.timestamp)
                .expect("decode");
            decode_call_ms.push(d0.elapsed().as_secs_f64() * 1000.0);
            while let Some(p) = decoder.poll_output() {
                decoded += 1;
                if let Some(t0) = at.lock().unwrap().get(&p.timestamp).copied() {
                    total_ms.push((qpc_100ns() - t0) as f64 / 10_000.0);
                }
            }
        }
        decoded += decoder.drain().expect("decoder drain").len() as u32;
        (decoded, decoder.is_hardware(), total_ms, decode_call_ms)
    });
    let pictures: Vec<Vec<u8>> = (0..8).map(|n| picture(width, height, n * 20)).collect();
    let duration = 10_000_000 / i64::from(fps);
    let interval = Duration::from_nanos(1_000_000_000 / u64::from(fps));
    let mut encode_ms = Vec::new();
    let (mut skipped, mut bytes) = (0u32, 0usize);
    let start = Instant::now();
    let mut handle = |frame: EncodedFrame| {
        if let Some(t0) = submitted_at.lock().unwrap().get(&frame.timestamp).copied() {
            encode_ms.push((qpc_100ns() - t0) as f64 / 10_000.0);
        }
        bytes += frame.data.len();
        tx.send(frame).expect("decoder thread");
    };
    for n in 0..frames {
        let due = start + interval * n;
        while Instant::now() < due {
            while let Some(f) = encoder.poll_output().expect("encoder output") {
                handle(f);
            }
            std::thread::sleep(Duration::from_micros(500));
        }
        if !encoder.ready().expect("encoder events") {
            skipped += 1;
            continue;
        }
        let ts = i64::from(n) * duration;
        submitted_at.lock().unwrap().insert(ts, qpc_100ns());
        encoder
            .encode(
                EncoderInput::Nv12(&pictures[n as usize % pictures.len()]),
                ts,
                duration,
            )
            .expect("encode");
    }
    for f in encoder.drain().expect("drain") {
        handle(f);
    }
    let wall = start.elapsed().as_secs_f64();
    drop(handle);
    drop(tx);
    let (decoded, dxva, mut total_ms, mut decode_call_ms) = decoder_thread.join().unwrap();
    println!(
        "TURBO_PANCAKE_VIDEO {{\"mode\":\"{width}x{height}@{fps}\",\"encoder\":{:?},\"decoder_dxva\":{dxva},\"adapter\":{:?},\"frames\":{frames},\"skipped\":{skipped},\"decoded\":{decoded},\"fps\":{:.1},\"kbps\":{:.0},\"encode_ms_p50\":{:.2},\"encode_ms_p95\":{:.2},\"encode_decode_ms_p50\":{:.2},\"encode_decode_ms_p95\":{:.2},\"encode_decode_ms_p99\":{:.2},\"decode_call_ms_p50\":{:.2},\"decode_call_ms_p95\":{:.2}}}",
        encoder.name(),
        adapter_name(&device),
        decoded as f64 / wall,
        bytes as f64 * 8.0 / wall / 1000.0,
        percentile(&mut encode_ms, 0.5),
        percentile(&mut encode_ms, 0.95),
        percentile(&mut total_ms, 0.5),
        percentile(&mut total_ms, 0.95),
        percentile(&mut total_ms, 0.99),
        percentile(&mut decode_call_ms, 0.5),
        percentile(&mut decode_call_ms, 0.95),
    );
    assert_eq!(decoded + skipped, frames, "every submitted frame decoded");
    assert!(
        skipped <= frames / 20,
        "encoder kept up with {fps} fps: {skipped} skipped"
    );
}

/// S4 previews: 50 preview streams (320×180, 10 fps) decoded on one thread with the OS
/// decoder without DXVA, as the client's preview decoder does. Each stream gets 3 s of
/// frames; the time spent is the share of one core 50 live previews cost.
#[test]
fn fifty_preview_streams_on_one_thread() {
    startup().expect("Media Foundation");
    let (device, _context, _) = create_device().expect("D3D11 device");
    let (w, h, fps, seconds) = (320u32, 180u32, 10u32, 3u32);
    let mut config = EncoderConfig::new(w, h, fps, 200);
    config.gop_frames = fps * 4;
    let mut encoder = match H264Encoder::new(&device, &config) {
        Ok(e) => e,
        Err(VideoError::NoCodec(what)) => {
            println!("SKIP: no H.264 {what}");
            return;
        }
        Err(e) => panic!("encoder: {e}"),
    };
    let duration = 10_000_000 / i64::from(fps);
    let mut frames = Vec::new();
    for n in 0..fps * seconds {
        wait_ready(&mut encoder);
        encoder
            .encode(
                EncoderInput::Nv12(&picture(w, h, n)),
                i64::from(n) * duration,
                duration,
            )
            .expect("encode");
        while let Some(f) = encoder.poll_output().expect("output") {
            frames.push(f);
        }
    }
    frames.extend(encoder.drain().expect("drain"));
    let streams = 50;
    let mut decoders: Vec<H264Decoder> = (0..streams)
        .map(|_| H264Decoder::new(None).expect("decoder"))
        .collect();
    let start = Instant::now();
    let mut decoded = 0usize;
    // Round robin like live streams: frame n of every stream, then frame n + 1.
    for f in &frames {
        for d in decoders.iter_mut() {
            d.decode(&f.data, f.timestamp).expect("decode");
            while d.poll_output().is_some() {
                decoded += 1;
            }
        }
    }
    let busy = start.elapsed().as_secs_f64();
    for d in decoders.iter_mut() {
        decoded += d.drain().expect("drain").len();
    }
    let core_share = busy / f64::from(seconds);
    println!(
        "TURBO_PANCAKE_PREVIEWS {{\"streams\":{streams},\"size\":\"{w}x{h}\",\"fps\":{fps},\"frames_per_stream\":{},\"decoded\":{decoded},\"decode_s\":{busy:.3},\"share_of_one_core\":{core_share:.3},\"ms_per_frame\":{:.3}}}",
        frames.len(),
        busy * 1000.0 / (frames.len() * streams) as f64
    );
    assert_eq!(
        decoded,
        frames.len() * streams,
        "every frame of every stream"
    );
}
