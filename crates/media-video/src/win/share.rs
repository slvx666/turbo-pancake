//! The sending side of a screen share on one thread: capture → GPU convert → encoder at a
//! fixed frame rate, newest frame wins (spec §5.1 SHOULD: no growing queues, prefer the
//! fresh frame). Encoded access units go to the caller's `deliver` callback, which must
//! not block; a frame it could not take makes the next frame a keyframe.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::capture::{Converter, MonitorCapture};
use super::{EncoderInput, H264Encoder, adapter_name, create_device, qpc_100ns, startup};
use crate::{
    EncodedFrame, EncoderConfig, ShareConfig, ShareSource, ShareStats, VideoError, percentile,
    synthetic_nv12,
};

/// Called with every encoded access unit; `false` = could not deliver (dropped).
pub type Deliver = Box<dyn FnMut(EncodedFrame) -> bool + Send>;

pub struct ScreenShare {
    stop: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
    bitrate: Arc<Mutex<Option<u32>>>,
    stats: Arc<Mutex<ShareStats>>,
    thread: Option<JoinHandle<()>>,
}

impl ScreenShare {
    /// Starts capturing and encoding; returns once the pipeline is running (or why not).
    pub fn start(config: ShareConfig, deliver: Deliver) -> Result<Self, VideoError> {
        EncoderConfig::new(config.width, config.height, config.fps, config.bitrate_kbps)
            .validate()?;
        let stop = Arc::new(AtomicBool::new(false));
        let keyframe = Arc::new(AtomicBool::new(false));
        let bitrate = Arc::new(Mutex::new(None));
        let stats = Arc::new(Mutex::new(ShareStats::default()));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let worker = Worker {
            config,
            deliver,
            stop: stop.clone(),
            keyframe: keyframe.clone(),
            bitrate: bitrate.clone(),
            stats: stats.clone(),
        };
        let thread = std::thread::Builder::new()
            .name("tp-screen-share".into())
            .spawn(move || worker.run(ready_tx))
            .map_err(|e| VideoError::Os {
                context: "share thread",
                message: e.to_string(),
            })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop,
                keyframe,
                bitrate,
                stats,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(VideoError::Os {
                    context: "share thread",
                    message: "exited during start".into(),
                })
            }
        }
    }

    /// The next encoded frame will be a keyframe.
    pub fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }

    pub fn set_bitrate(&self, kbps: u32) {
        if let Ok(mut b) = self.bitrate.lock() {
            *b = Some(kbps);
        }
    }

    pub fn stats(&self) -> ShareStats {
        self.stats.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// True while the pipeline runs (false after an error, e.g. a resolution change).
    pub fn is_running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for ScreenShare {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Worker {
    config: ShareConfig,
    deliver: Deliver,
    stop: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
    bitrate: Arc<Mutex<Option<u32>>>,
    stats: Arc<Mutex<ShareStats>>,
}

enum Source {
    Monitor {
        capture: MonitorCapture,
        converter: Converter,
    },
    Synthetic {
        n: u32,
    },
}

impl Worker {
    fn run(mut self, ready: std::sync::mpsc::Sender<Result<(), VideoError>>) {
        let setup = (|| -> Result<(Source, H264Encoder, ShareStats), VideoError> {
            startup()?;
            let (device, context, _) = create_device()?;
            let c = &self.config;
            let (source, source_size) = match c.source {
                ShareSource::Monitor(index) => {
                    let capture = MonitorCapture::start(&device, &context, index)?;
                    let size = capture.size();
                    let converter = Converter::new(
                        &device,
                        &context,
                        capture.texture(),
                        size,
                        (c.width, c.height),
                        c.fps,
                        4,
                    )?;
                    (Source::Monitor { capture, converter }, size)
                }
                ShareSource::Synthetic => (Source::Synthetic { n: 0 }, (c.width, c.height)),
            };
            let mut ec = EncoderConfig::new(c.width, c.height, c.fps, c.bitrate_kbps);
            // Keyframes on request (new viewer, loss) plus one every few seconds.
            ec.gop_frames = c.fps * 4;
            ec.require_hardware = c.require_hardware;
            let encoder = H264Encoder::new(&device, &ec)?;
            let stats = ShareStats {
                source_width: source_size.0,
                source_height: source_size.1,
                width: c.width,
                height: c.height,
                fps: c.fps,
                encoder: encoder.name().to_owned(),
                hardware: encoder.is_hardware(),
                adapter: adapter_name(&device),
                ..Default::default()
            };
            Ok((source, encoder, stats))
        })();
        let (mut source, mut encoder, mut st) = match setup {
            Ok(s) => s,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let _ = ready.send(Ok(()));
        if let Err(e) = self.pump(&mut source, &mut encoder, &mut st) {
            st.error = Some(e.to_string());
        }
        // Leftovers are of no use to a stopped share.
        let _ = encoder.drain();
        self.publish(&st);
    }

    fn pump(
        &mut self,
        source: &mut Source,
        encoder: &mut H264Encoder,
        st: &mut ShareStats,
    ) -> Result<(), VideoError> {
        let (w, h, fps) = (self.config.width, self.config.height, self.config.fps);
        let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
        let duration_100ns = 10_000_000 / i64::from(fps);
        let mut next_tick = Instant::now();
        let mut fresh: Option<i64> = None;
        let mut have_frame = matches!(source, Source::Synthetic { .. });
        let mut last_ts = 0i64;
        let mut submitted: Vec<(i64, i64, bool)> = Vec::new(); // (ts, submit qpc, fresh)
        let mut encode_ms = Vec::new();
        let mut capture_ms = Vec::new();
        let mut last_publish = Instant::now();
        let (mut window_frames, mut window_bytes, mut window_captured) = (0u64, 0u64, 0u64);
        while !self.stop.load(Ordering::Relaxed) {
            if let Source::Monitor { capture, .. } = source {
                capture.wait(
                    next_tick
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(2)),
                );
                if let Some((t, arrived)) = capture.take_latest()? {
                    fresh = Some(t);
                    have_frame = true;
                    st.captured_frames += arrived;
                    window_captured += arrived;
                }
            } else {
                std::thread::sleep(
                    next_tick
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(2)),
                );
            }
            if let Some(kbps) = self.bitrate.lock().ok().and_then(|mut b| b.take()) {
                encoder.set_bitrate(kbps)?;
                st.bitrate_kbps_target = kbps;
            }
            while let Some(frame) = encoder.poll_output()? {
                let now = qpc_100ns();
                if let Some(i) = submitted.iter().position(|s| s.0 == frame.timestamp) {
                    let (ts, at, was_fresh) = submitted.swap_remove(i);
                    encode_ms.push((now - at) as f64 / 10_000.0);
                    if was_fresh {
                        capture_ms.push((now - ts) as f64 / 10_000.0);
                    }
                }
                st.encoded_frames += 1;
                st.bytes += frame.data.len() as u64;
                st.keyframes += u64::from(frame.keyframe);
                window_frames += 1;
                window_bytes += frame.data.len() as u64;
                if !(self.deliver)(frame) {
                    st.dropped_delivery += 1;
                    self.keyframe.store(true, Ordering::Relaxed);
                }
            }
            let now = Instant::now();
            if now >= next_tick {
                next_tick += interval;
                if now > next_tick + interval {
                    // Fell behind by more than a frame: skip ahead instead of bursting.
                    let behind = ((now - next_tick).as_secs_f64() / interval.as_secs_f64()) as u32;
                    st.ticks_skipped += u64::from(behind);
                    next_tick += interval * behind;
                }
                if have_frame {
                    if !encoder.ready()? {
                        st.dropped_encoder_busy += 1;
                    } else {
                        if self.keyframe.swap(false, Ordering::Relaxed) {
                            encoder.request_keyframe()?;
                            st.keyframe_requests += 1;
                        }
                        let capture_time = fresh.take();
                        let ts = capture_time.unwrap_or_else(qpc_100ns).max(last_ts + 1);
                        last_ts = ts;
                        match source {
                            Source::Monitor { converter, .. } => {
                                let nv12 = converter.convert()?;
                                encoder.encode(EncoderInput::Texture(&nv12), ts, duration_100ns)?;
                            }
                            Source::Synthetic { n } => {
                                let bytes = synthetic_nv12(w, h, *n);
                                *n += 1;
                                encoder.encode(EncoderInput::Nv12(&bytes), ts, duration_100ns)?;
                            }
                        }
                        let is_fresh =
                            capture_time.is_some() || matches!(source, Source::Synthetic { .. });
                        submitted.push((ts, qpc_100ns(), is_fresh));
                        // Outputs that never come back (should not happen) must not pile up.
                        if submitted.len() > 64 {
                            submitted.remove(0);
                        }
                        st.submitted_frames += 1;
                        st.submitted_repeat += u64::from(!is_fresh);
                    }
                }
            }
            if last_publish.elapsed() >= Duration::from_millis(500) {
                let secs = last_publish.elapsed().as_secs_f64();
                st.encoded_fps = window_frames as f64 / secs;
                st.captured_fps = window_captured as f64 / secs;
                st.bitrate_kbps = window_bytes as f64 * 8.0 / secs / 1000.0;
                st.encode_ms_p50 = percentile(&mut encode_ms, 0.5);
                st.encode_ms_p95 = percentile(&mut encode_ms, 0.95);
                st.capture_to_encoded_ms_p50 = percentile(&mut capture_ms, 0.5);
                st.capture_to_encoded_ms_p95 = percentile(&mut capture_ms, 0.95);
                encode_ms.clear();
                capture_ms.clear();
                (window_frames, window_bytes, window_captured) = (0, 0, 0);
                last_publish = Instant::now();
                self.publish(st);
            }
        }
        Ok(())
    }

    fn publish(&self, st: &ShareStats) {
        if let Ok(mut s) = self.stats.lock() {
            s.clone_from(st);
        }
    }
}
