//! System audio excludes the messenger's own sound (spec §5.1, S3 step 6). Three quiet
//! tones (−34 dBFS) play at once: 1000 Hz from this process and 400 Hz from a child
//! process (both in the messenger's process tree → must be absent from the capture), and
//! 600 Hz from a process outside the tree (started through WMI, whose parent is the WMI
//! service → must be captured). Needs a default output device (skips without one).

#![cfg(windows)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use turbo_pancake_media_audio::system::{SystemAudioCapture, SystemAudioFrame};

const AMPLITUDE: f32 = 0.02;

/// Plays a sine on the default output until the returned stream is dropped.
fn play(freq: f32) -> Option<cpal::Stream> {
    let device = cpal::default_host().default_output_device()?;
    let config = device.default_output_config().ok()?;
    let rate = config.sample_rate() as f32;
    let channels = usize::from(config.channels());
    let mut phase = 0.0f32;
    let stream = device
        .build_output_stream(
            config.into(),
            move |data: &mut [f32], _| {
                for frame in data.chunks_mut(channels) {
                    let v = (phase * std::f32::consts::TAU).sin() * AMPLITUDE;
                    phase = (phase + freq / rate) % 1.0;
                    frame.fill(v);
                }
            },
            |_| {},
            None,
        )
        .ok()?;
    stream.play().ok()?;
    Some(stream)
}

/// Helper process modes (this test binary run again): play one tone for 4 s.
#[test]
#[ignore]
fn tone_player_400() {
    let _stream = play(400.0);
    std::thread::sleep(Duration::from_secs(4));
}

#[test]
#[ignore]
fn tone_player_600() {
    let _stream = play(600.0);
    std::thread::sleep(Duration::from_secs(4));
}

/// Starts `tone_player_600` outside our process tree (WMI creates it).
fn start_outside_tree() -> bool {
    let exe = std::env::current_exe().unwrap();
    let cmd = format!(
        "Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{{CommandLine='\"{}\" --ignored --exact tone_player_600'}} | Out-Null",
        exe.display()
    );
    std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &cmd])
        .status()
        .is_ok_and(|s| s.success())
}

/// Power of `freq` in `x` (Goertzel), normalised by length.
fn power(x: &[f32], freq: f32, rate: f32) -> f32 {
    let w = std::f32::consts::TAU * freq / rate;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for &v in x {
        let s0 = v + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - coeff * s1 * s2) / (x.len() as f32).powi(2)
}

#[test]
fn own_sound_is_excluded_other_processes_are_captured() {
    if cpal::default_host().default_output_device().is_none() {
        println!("SKIP: no output device");
        return;
    }
    let frames: Arc<Mutex<Vec<SystemAudioFrame>>> = Arc::default();
    let sink = frames.clone();
    let capture = match SystemAudioCapture::start(move |f| sink.lock().unwrap().push(f)) {
        Ok(c) => c,
        Err(e) => panic!("process loopback: {e}"),
    };
    let Some(own) = play(1000.0) else {
        println!("SKIP: cannot open the output device");
        return;
    };
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "tone_player_400"])
        .spawn()
        .unwrap();
    assert!(start_outside_tree(), "WMI process start");
    std::thread::sleep(Duration::from_millis(1000));
    // Capture only while all three tones play.
    frames.lock().unwrap().clear();
    std::thread::sleep(Duration::from_secs(2));
    let got = std::mem::take(&mut *frames.lock().unwrap());
    let stats = capture.stats();
    let _ = child.wait();
    drop(own);
    capture.stop();

    // Decode stereo → interleaved, average to mono.
    let mut decoder = opus::Decoder::new(48_000, opus::Channels::Stereo).unwrap();
    let mut pcm = Vec::new();
    let mut buf = vec![0f32; 960 * 2];
    for f in &got {
        let n = decoder.decode_float(&f.payload, &mut buf, false).unwrap();
        pcm.extend(buf[..n * 2].chunks(2).map(|c| (c[0] + c[1]) / 2.0));
    }
    assert!(got.len() >= 50, "{} frames in 2 s; {stats:?}", got.len());
    let p = |f: f32| power(&pcm, f, 48_000.0);
    let (p400, p600, p1000) = (p(400.0), p(600.0), p(1000.0));
    let tone = (AMPLITUDE / 2.0).powi(2); // power of a pure tone of that amplitude
    let below = |x: f32| 10.0 * (p600 / x.max(1e-20)).log10();
    println!(
        "TURBO_PANCAKE_SYSTEM_AUDIO frames={} p600_outside={:.2e} p1000_own={:.2e} p400_child={:.2e} (pure tone {:.2e}) own_below_db={:.1} child_below_db={:.1} {stats:?}",
        got.len(),
        p600,
        p1000,
        p400,
        tone,
        below(p1000),
        below(p400),
    );
    assert!(p600 > tone * 0.25, "a process outside the tree is captured");
    assert!(
        p1000 < p600 / 100.0,
        "our own tone is excluded (≥ 20 dB below)"
    );
    assert!(
        p400 < p600 / 100.0,
        "our child's tone is excluded (≥ 20 dB below)"
    );
}
