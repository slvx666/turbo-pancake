//! The sending pipeline (S3 step 3): a synthetic share runs at its frame rate, everything
//! it encodes decodes, keyframe requests are honoured; with `--ignored`, a real capture of
//! the secondary monitor (manual: needs a desktop session and a second monitor).

#![cfg(windows)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use turbo_pancake_media_video::win::{H264Decoder, startup};
use turbo_pancake_media_video::{
    EncodedFrame, ScreenShare, ShareConfig, ShareSource, VideoError, monitors,
};

fn run(config: ShareConfig, seconds: f64) -> Option<(Vec<EncodedFrame>, ScreenShare)> {
    let frames = Arc::new(Mutex::new(Vec::new()));
    let sink = frames.clone();
    let share = match ScreenShare::start(
        config,
        Box::new(move |f| {
            sink.lock().unwrap().push(f);
            true
        }),
    ) {
        Ok(s) => s,
        Err(VideoError::NoCodec(what)) => {
            println!("SKIP: no H.264 {what}");
            return None;
        }
        Err(e) => panic!("share: {e}"),
    };
    std::thread::sleep(Duration::from_secs_f64(seconds / 2.0));
    share.request_keyframe();
    std::thread::sleep(Duration::from_secs_f64(seconds / 2.0));
    let got = std::mem::take(&mut *frames.lock().unwrap());
    Some((got, share))
}

fn decode_all(frames: &[EncodedFrame]) -> usize {
    startup().unwrap();
    let mut decoder = H264Decoder::new(None).unwrap();
    let mut n = 0;
    for f in frames {
        decoder.decode(&f.data, f.timestamp).unwrap();
        while decoder.poll_output().is_some() {
            n += 1;
        }
    }
    n + decoder.drain().unwrap().len()
}

#[test]
fn synthetic_share_runs_at_its_frame_rate() {
    let mut config = ShareConfig::hd60(ShareSource::Synthetic);
    config.width = 1280;
    config.height = 720;
    let Some((frames, share)) = run(config, 2.0) else {
        return;
    };
    let stats = share.stats();
    share.stop();
    println!("{} frames; {stats:?}", frames.len());
    assert!(stats.error.is_none(), "{:?}", stats.error);
    // 60 fps for 2 s; CI machines without a GPU encode in software, so be lenient.
    assert!(frames.len() >= 60, "{} frames in 2 s", frames.len());
    assert!(frames[0].keyframe, "starts with a keyframe");
    assert!(stats.keyframe_requests >= 1);
    assert!(
        frames.iter().skip(1).any(|f| f.keyframe),
        "requested keyframe"
    );
    let decoded = decode_all(&frames);
    assert_eq!(decoded, frames.len(), "every frame decodes");
}

/// Manual: `cargo test -p turbo-pancake-media-video --release --test share -- --ignored
/// --nocapture` with a second monitor (only that monitor is captured; nothing is stored).
#[test]
#[ignore]
fn capture_secondary_monitor() {
    let list = monitors();
    println!("{list:?}");
    let Some(m) = list.iter().find(|m| !m.primary) else {
        println!("SKIP: no secondary monitor");
        return;
    };
    let Some((frames, share)) = run(ShareConfig::hd60(ShareSource::Monitor(m.index)), 4.0) else {
        return;
    };
    let stats = share.stats();
    share.stop();
    println!("{} frames; {stats:?}", frames.len());
    assert!(stats.error.is_none(), "{:?}", stats.error);
    assert!(frames.len() >= 200, "{} frames in 4 s", frames.len());
    assert_eq!(decode_all(&frames), frames.len());
}

/// Decoded picture sizes in order of `frames`.
fn decoded_sizes(frames: &[EncodedFrame]) -> Vec<(u32, u32)> {
    startup().unwrap();
    let mut decoder = H264Decoder::new(None).unwrap();
    let mut out = Vec::new();
    for f in frames {
        decoder.decode(&f.data, f.timestamp).unwrap();
        while let Some(p) = decoder.poll_output() {
            out.push((p.width, p.height));
        }
    }
    out.extend(decoder.drain().unwrap().iter().map(|p| (p.width, p.height)));
    out
}

/// S4: a preview layer at ≈ 10 fps / 320 px next to the high layer, the same capture
/// timestamps, its own keyframe requests; the high layer changes size while running.
#[test]
fn preview_layer_and_reconfiguration() {
    let mut config = ShareConfig::hd60(ShareSource::Synthetic);
    (config.width, config.height, config.fps) = (1280, 720, 30);
    let main = Arc::new(Mutex::new(Vec::new()));
    let preview = Arc::new(Mutex::new(Vec::new()));
    let (m, p) = (main.clone(), preview.clone());
    let share = match ScreenShare::start_layers(
        config,
        Box::new(move |f| {
            m.lock().unwrap().push(f);
            true
        }),
        Some(Box::new(move |f| {
            p.lock().unwrap().push(f);
            true
        })),
    ) {
        Ok(s) => s,
        Err(VideoError::NoCodec(what)) => {
            println!("SKIP: no H.264 {what}");
            return;
        }
        Err(e) => panic!("share: {e}"),
    };
    std::thread::sleep(Duration::from_millis(1500));
    share.request_preview_keyframe();
    share.reconfigure(640, 360, 30);
    std::thread::sleep(Duration::from_millis(1500));
    let stats = share.stats();
    share.stop();
    let main = std::mem::take(&mut *main.lock().unwrap());
    let preview = std::mem::take(&mut *preview.lock().unwrap());
    println!(
        "main {} preview {} frames; {stats:?}",
        main.len(),
        preview.len()
    );
    assert!(stats.error.is_none(), "{:?}", stats.error);
    assert_eq!((stats.preview_width, stats.preview_height), (320, 180));
    // 10 fps for 3 s (lenient for software encoders on CI).
    assert!((20..=34).contains(&preview.len()), "{}", preview.len());
    assert!(preview[0].keyframe);
    assert!(
        preview.iter().skip(1).any(|f| f.keyframe),
        "requested keyframe"
    );
    // Every preview frame has the capture time of a main frame.
    let main_ts: std::collections::HashSet<i64> = main.iter().map(|f| f.timestamp).collect();
    assert!(preview.iter().all(|f| main_ts.contains(&f.timestamp)));
    assert!(decoded_sizes(&preview).iter().all(|s| *s == (320, 180)));
    // The high layer: 1280×720, then 640×360 after the reconfiguration (new keyframe).
    assert_eq!(stats.reconfigurations, 1);
    assert_eq!((stats.width, stats.height), (640, 360));
    let sizes = decoded_sizes(&main);
    assert_eq!(sizes.len(), main.len(), "every main frame decodes");
    let first_small = sizes
        .iter()
        .position(|s| *s == (640, 360))
        .expect("resized");
    assert!(sizes[..first_small].iter().all(|s| *s == (1280, 720)));
    assert!(sizes[first_small..].iter().all(|s| *s == (640, 360)));
}
