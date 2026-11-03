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
