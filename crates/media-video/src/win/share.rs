//! The sending side of a screen share on one thread: capture → GPU convert → encoder at a
//! fixed frame rate, newest frame wins (spec §5.1 SHOULD: no growing queues, prefer the
//! fresh frame). Encoded access units go to the caller's `deliver` callback, which must
//! not block; a frame it could not take makes the next frame a keyframe.
//!
//! With a preview layer (S4) the same captured picture is also scaled to ≈ 320 px and
//! encoded by a second encoder at the preview rate (every n-th tick, same capture
//! timestamp). The high layer can change size and frame rate while running
//! ([`ScreenShare::reconfigure`]: a new converter and encoder, starting with a keyframe).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::capture::{Converter, MonitorCapture};
use super::{
    EncoderInput, H264Encoder, ID3D11Device, adapter_name, create_device, qpc_100ns, startup,
};
use crate::adapt::fit;
use crate::{
    EncodedFrame, EncoderConfig, ShareConfig, ShareSource, ShareStats, VideoError, percentile,
    synthetic_nv12,
};
use windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext;

/// Called with every encoded access unit; `false` = could not deliver (dropped).
pub type Deliver = Box<dyn FnMut(EncodedFrame) -> bool + Send>;

#[derive(Default)]
struct Controls {
    bitrate: Option<u32>,
    reconfigure: Option<(u32, u32, u32)>,
}

pub struct ScreenShare {
    stop: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
    preview_keyframe: Arc<AtomicBool>,
    controls: Arc<Mutex<Controls>>,
    stats: Arc<Mutex<ShareStats>>,
    thread: Option<JoinHandle<()>>,
}

impl ScreenShare {
    /// Starts capturing and encoding (high layer only); returns once the pipeline is
    /// running (or why not).
    pub fn start(config: ShareConfig, deliver: Deliver) -> Result<Self, VideoError> {
        Self::start_layers(
            ShareConfig {
                preview: None,
                ..config
            },
            deliver,
            None,
        )
    }

    /// [`ScreenShare::start`] plus the preview layer (if `config.preview` and `preview`).
    pub fn start_layers(
        config: ShareConfig,
        deliver: Deliver,
        preview: Option<Deliver>,
    ) -> Result<Self, VideoError> {
        EncoderConfig::new(config.width, config.height, config.fps, config.bitrate_kbps)
            .validate()?;
        let stop = Arc::new(AtomicBool::new(false));
        let keyframe = Arc::new(AtomicBool::new(false));
        let preview_keyframe = Arc::new(AtomicBool::new(false));
        let controls = Arc::new(Mutex::new(Controls::default()));
        let stats = Arc::new(Mutex::new(ShareStats::default()));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let preview = if config.preview.is_some() {
            preview
        } else {
            None
        };
        let worker = Worker {
            config,
            deliver,
            preview_deliver: preview,
            stop: stop.clone(),
            keyframe: keyframe.clone(),
            preview_keyframe: preview_keyframe.clone(),
            controls: controls.clone(),
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
                preview_keyframe,
                controls,
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

    /// The next preview frame will be a keyframe.
    pub fn request_preview_keyframe(&self) {
        self.preview_keyframe.store(true, Ordering::Relaxed);
    }

    pub fn set_bitrate(&self, kbps: u32) {
        if let Ok(mut c) = self.controls.lock() {
            c.bitrate = Some(kbps);
        }
    }

    /// New size (fitted to the source, even) and frame rate of the high layer.
    pub fn reconfigure(&self, width: u32, height: u32, fps: u32) {
        if let Ok(mut c) = self.controls.lock() {
            c.reconfigure = Some((width, height, fps.clamp(1, 240)));
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
    preview_deliver: Option<Deliver>,
    stop: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
    preview_keyframe: Arc<AtomicBool>,
    controls: Arc<Mutex<Controls>>,
    stats: Arc<Mutex<ShareStats>>,
}

enum Source {
    Monitor(MonitorCapture),
    Synthetic { n: u32 },
}

/// One encoded layer: converter (monitor source only) + encoder at a size.
struct Layer {
    converter: Option<Converter>,
    encoder: H264Encoder,
    size: (u32, u32),
}

impl Layer {
    #[allow(clippy::too_many_arguments)]
    fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        source: &Source,
        source_size: (u32, u32),
        size: (u32, u32),
        fps: u32,
        kbps: u32,
        require_hardware: bool,
    ) -> Result<Self, VideoError> {
        let converter = match source {
            Source::Monitor(capture) => Some(Converter::new(
                device,
                context,
                capture.texture(),
                source_size,
                size,
                fps,
                4,
            )?),
            Source::Synthetic { .. } => None,
        };
        let mut ec = EncoderConfig::new(size.0, size.1, fps, kbps);
        // Keyframes on request (new viewer, loss, layer switch) plus one every few seconds.
        ec.gop_frames = fps * 4;
        ec.require_hardware = require_hardware;
        let encoder = H264Encoder::new(device, &ec)?;
        Ok(Self {
            converter,
            encoder,
            size,
        })
    }

    /// Encodes the current picture; `n` drives the synthetic pattern.
    fn encode(&mut self, n: Option<u32>, ts: i64, duration: i64) -> Result<(), VideoError> {
        match (&mut self.converter, n) {
            (Some(c), _) => {
                let nv12 = c.convert()?;
                self.encoder
                    .encode(EncoderInput::Texture(&nv12), ts, duration)
            }
            (None, n) => {
                let bytes = synthetic_nv12(self.size.0, self.size.1, n.unwrap_or(0));
                self.encoder
                    .encode(EncoderInput::Nv12(&bytes), ts, duration)
            }
        }
    }
}

impl Worker {
    fn run(mut self, ready: std::sync::mpsc::Sender<Result<(), VideoError>>) {
        let setup = (|| {
            startup()?;
            let (device, context, _) = create_device()?;
            let c = self.config;
            let (source, source_size) = match c.source {
                ShareSource::Monitor(index) => {
                    let capture = MonitorCapture::start(&device, &context, index)?;
                    let size = capture.size();
                    (Source::Monitor(capture), size)
                }
                ShareSource::Synthetic => (Source::Synthetic { n: 0 }, (c.width, c.height)),
            };
            let size = fit(source_size, (c.width, c.height));
            let main = Layer::new(
                &device,
                &context,
                &source,
                source_size,
                size,
                c.fps,
                c.bitrate_kbps,
                c.require_hardware,
            )?;
            let preview = match (c.preview, &self.preview_deliver) {
                (Some(p), Some(_)) => {
                    let psize = fit(source_size, (p.max_side, p.max_side));
                    Some(Layer::new(
                        &device,
                        &context,
                        &source,
                        source_size,
                        psize,
                        p.fps.clamp(1, c.fps),
                        p.bitrate_kbps,
                        false,
                    )?)
                }
                _ => None,
            };
            let stats = ShareStats {
                source_width: source_size.0,
                source_height: source_size.1,
                width: size.0,
                height: size.1,
                fps: c.fps,
                encoder: main.encoder.name().to_owned(),
                hardware: main.encoder.is_hardware(),
                adapter: adapter_name(&device),
                bitrate_kbps_target: c.bitrate_kbps,
                preview_width: preview.as_ref().map_or(0, |p| p.size.0),
                preview_height: preview.as_ref().map_or(0, |p| p.size.1),
                preview_encoder: preview
                    .as_ref()
                    .map(|p| p.encoder.name().to_owned())
                    .unwrap_or_default(),
                ..Default::default()
            };
            Ok::<_, VideoError>((device, context, source, source_size, main, preview, stats))
        })();
        let (device, context, mut source, source_size, mut main, mut preview, mut st) = match setup
        {
            Ok(s) => s,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let _ = ready.send(Ok(()));
        let mut env = Env {
            device: &device,
            context: &context,
            source_size,
        };
        if let Err(e) = self.pump(&mut env, &mut source, &mut main, &mut preview, &mut st) {
            st.error = Some(e.to_string());
        }
        // Leftovers are of no use to a stopped share.
        let _ = main.encoder.drain();
        if let Some(p) = preview.as_mut() {
            let _ = p.encoder.drain();
        }
        self.publish(&st);
    }

    fn pump(
        &mut self,
        env: &mut Env<'_>,
        source: &mut Source,
        main: &mut Layer,
        preview: &mut Option<Layer>,
        st: &mut ShareStats,
    ) -> Result<(), VideoError> {
        let mut fps = self.config.fps;
        let mut interval = Duration::from_secs_f64(1.0 / f64::from(fps));
        let mut duration_100ns = 10_000_000 / i64::from(fps);
        let preview_fps = self.config.preview.map_or(1, |p| p.fps.clamp(1, fps));
        let preview_interval = 10_000_000 / i64::from(preview_fps);
        let mut last_preview_ts: Option<i64> = None;
        let mut next_tick = Instant::now();
        let mut fresh: Option<i64> = None;
        let mut have_frame = matches!(source, Source::Synthetic { .. });
        let mut last_ts = 0i64;
        let mut submitted: Vec<(i64, i64, bool)> = Vec::new(); // (ts, submit qpc, fresh)
        let mut encode_ms = Vec::new();
        let mut capture_ms = Vec::new();
        let mut last_publish = Instant::now();
        let (mut window_frames, mut window_bytes, mut window_captured) = (0u64, 0u64, 0u64);
        let mut window_preview_bytes = 0u64;
        while !self.stop.load(Ordering::Relaxed) {
            if let Source::Monitor(capture) = source {
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
            let (bitrate, reconf) = self
                .controls
                .lock()
                .map(|mut c| (c.bitrate.take(), c.reconfigure.take()))
                .unwrap_or_default();
            if let Some((w, h, new_fps)) = reconf {
                let size = fit(env.source_size, (w, h));
                if size != main.size || new_fps != fps {
                    // Frames still in the old encoder go out (the preview has the same
                    // timestamps; viewers decode them before the new keyframe).
                    for frame in main.encoder.drain()? {
                        st.encoded_frames += 1;
                        if !(self.deliver)(frame) {
                            st.dropped_delivery += 1;
                        }
                    }
                    let kbps = bitrate.unwrap_or(st.bitrate_kbps_target);
                    *main = Layer::new(
                        env.device,
                        env.context,
                        source,
                        env.source_size,
                        size,
                        new_fps,
                        kbps,
                        self.config.require_hardware,
                    )?;
                    fps = new_fps;
                    interval = Duration::from_secs_f64(1.0 / f64::from(fps));
                    duration_100ns = 10_000_000 / i64::from(fps);
                    (st.width, st.height, st.fps) = (size.0, size.1, fps);
                    st.bitrate_kbps_target = kbps;
                    st.reconfigurations += 1;
                    submitted.clear();
                }
            }
            if let Some(kbps) = bitrate {
                main.encoder.set_bitrate(kbps)?;
                st.bitrate_kbps_target = kbps;
            }
            while let Some(frame) = main.encoder.poll_output()? {
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
            if let (Some(p), Some(deliver)) = (preview.as_mut(), self.preview_deliver.as_mut()) {
                while let Some(frame) = p.encoder.poll_output()? {
                    st.preview_frames += 1;
                    window_preview_bytes += frame.data.len() as u64;
                    if !deliver(frame) {
                        self.preview_keyframe.store(true, Ordering::Relaxed);
                    }
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
                    let n = match source {
                        Source::Synthetic { n } => {
                            *n += 1;
                            Some(*n)
                        }
                        Source::Monitor(_) => None,
                    };
                    if !main.encoder.ready()? {
                        st.dropped_encoder_busy += 1;
                    } else {
                        if self.keyframe.swap(false, Ordering::Relaxed) {
                            main.encoder.request_keyframe()?;
                            st.keyframe_requests += 1;
                        }
                        let capture_time = fresh.take();
                        let ts = capture_time.unwrap_or_else(qpc_100ns).max(last_ts + 1);
                        last_ts = ts;
                        main.encode(n, ts, duration_100ns)?;
                        let is_fresh = capture_time.is_some() || n.is_some();
                        submitted.push((ts, qpc_100ns(), is_fresh));
                        // Outputs that never come back (should not happen) must not pile up.
                        if submitted.len() > 64 {
                            submitted.remove(0);
                        }
                        st.submitted_frames += 1;
                        st.submitted_repeat += u64::from(!is_fresh);
                        // The preview takes the same picture at its own, lower rate.
                        if let Some(p) = preview.as_mut()
                            && last_preview_ts
                                .is_none_or(|l| ts - l >= preview_interval - duration_100ns / 2)
                            && p.encoder.ready()?
                        {
                            if self.preview_keyframe.swap(false, Ordering::Relaxed) {
                                p.encoder.request_keyframe()?;
                            }
                            p.encode(n, ts, preview_interval)?;
                            last_preview_ts = Some(ts);
                        }
                    }
                }
            }
            if last_publish.elapsed() >= Duration::from_millis(500) {
                let secs = last_publish.elapsed().as_secs_f64();
                st.encoded_fps = window_frames as f64 / secs;
                st.captured_fps = window_captured as f64 / secs;
                st.bitrate_kbps = window_bytes as f64 * 8.0 / secs / 1000.0;
                st.preview_bitrate_kbps = window_preview_bytes as f64 * 8.0 / secs / 1000.0;
                st.encode_ms_p50 = percentile(&mut encode_ms, 0.5);
                st.encode_ms_p95 = percentile(&mut encode_ms, 0.95);
                st.capture_to_encoded_ms_p50 = percentile(&mut capture_ms, 0.5);
                st.capture_to_encoded_ms_p95 = percentile(&mut capture_ms, 0.95);
                encode_ms.clear();
                capture_ms.clear();
                (window_frames, window_bytes, window_captured) = (0, 0, 0);
                window_preview_bytes = 0;
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

/// What a reconfiguration needs to build a new layer.
struct Env<'a> {
    device: &'a ID3D11Device,
    context: &'a ID3D11DeviceContext,
    source_size: (u32, u32),
}
