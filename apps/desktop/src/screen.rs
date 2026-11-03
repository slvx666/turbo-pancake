//! Screen share in the client (S3): the viewer thread and the glue between the sending
//! pipeline (`turbo_pancake_media_video::ScreenShare`) and the call.
//!
//! ```text
//! call thread ─ on_video (watched SSRC only) ─▶ bounded channel ─▶ viewer thread
//!     FrameAssembler (waits for RTX repairs, skips to a keyframe after a loss)
//!     ─▶ H.264 decoder (OS, DXVA) ─▶ NV12 → RGBA ─▶ PictureSlot (newest picture wins)
//! UI timer ─ takes the newest picture ─▶ Slint image
//! ```
//!
//! The viewer asks the sharer for a keyframe (through the call) when it lost a frame,
//! when the decoder fails, or when no keyframe arrived for a while.
//!
//! RTP timestamps of a share are its capture times (90 kHz units of the performance
//! counter), so on one machine the viewer measures capture → shown latency directly.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use turbo_pancake_media_transport::call::KeyframeRequester;
use turbo_pancake_media_transport::video::{FrameAssembler, VideoIn};
use turbo_pancake_media_video::percentile;

/// Packets of the watched share waiting for the viewer: ~1 s of 8 Mbit/s video.
const VIEWER_QUEUE: usize = 1024;
/// At most one keyframe request per this interval.
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(300);

/// A decoded picture ready to show.
#[derive(Debug, Clone)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Increases with every new picture (the UI skips repeats).
    pub serial: u64,
}

/// The newest decoded picture; the UI takes it, older ones are overwritten unseen.
pub type PictureSlot = Arc<Mutex<Option<Picture>>>;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ViewerStats {
    pub packets: u64,
    pub frames_assembled: u64,
    /// Frames dropped as incomplete or undecodable until the next keyframe.
    pub frames_dropped: u64,
    pub frames_shown: u64,
    /// Decoded pictures (all frames are decoded; when behind, only the newest is shown).
    pub frames_decoded: u64,
    pub decode_errors: u64,
    pub keyframe_requests: u64,
    pub width: u32,
    pub height: u32,
    /// Shown pictures per second over the last ≈ 0.5 s.
    pub fps: f64,
    pub decode_ms_p50: f64,
    /// Capture → picture ready, from the share's capture timestamps; meaningful when both
    /// clients run on one machine (one clock).
    pub latency_ms_p50: f64,
    pub latency_ms_p95: f64,
    pub hardware_decoder: bool,
    pub error: Option<String>,
}

/// RTP timestamp of a capture time (100 ns units) at 90 kHz.
pub fn rtp_time(capture_100ns: i64) -> u32 {
    (capture_100ns as i128 * 9 / 1000) as u32
}

/// Now on the capture clock, 90 kHz, wrapping like RTP timestamps.
fn now_rtp() -> u32 {
    #[cfg(windows)]
    {
        rtp_time(turbo_pancake_media_video::win::qpc_100ns())
    }
    #[cfg(not(windows))]
    {
        0
    }
}

pub struct Viewer {
    tx: SyncSender<VideoIn>,
    stop: Arc<AtomicBool>,
    reset: Arc<AtomicBool>,
    stats: Arc<Mutex<ViewerStats>>,
    thread: Option<JoinHandle<()>>,
}

impl Viewer {
    /// `keyframes` names the stream being watched (set by the session when it changes).
    pub fn start(
        slot: PictureSlot,
        keyframes: Arc<Mutex<Option<KeyframeRequester>>>,
    ) -> std::io::Result<Self> {
        let (tx, rx) = sync_channel(VIEWER_QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let reset = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Mutex::new(ViewerStats::default()));
        let worker = Worker {
            rx,
            slot,
            keyframes,
            stop: stop.clone(),
            reset: reset.clone(),
            stats: stats.clone(),
        };
        let thread = std::thread::Builder::new()
            .name("tp-screen-view".into())
            .spawn(move || worker.run())?;
        Ok(Self {
            tx,
            stop,
            reset,
            stats,
            thread: Some(thread),
        })
    }

    /// Where the call thread puts packets of the watched share (`try_send`: full = lost).
    pub fn sender(&self) -> SyncSender<VideoIn> {
        self.tx.clone()
    }

    /// A different stream follows (another share, a new media session).
    pub fn reset(&self) {
        self.reset.store(true, Ordering::Relaxed);
    }

    pub fn stats(&self) -> ViewerStats {
        self.stats.lock().map(|s| s.clone()).unwrap_or_default()
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

impl Drop for Viewer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// A decoded picture before colour conversion (built by the OS decoder: Windows only yet).
#[cfg_attr(not(windows), allow(dead_code))]
struct Nv12Picture {
    width: u32,
    height: u32,
    nv12: Vec<u8>,
    /// RTP timestamp of the frame.
    timestamp: u32,
}

/// The OS decoder behind the viewer.
trait PictureDecoder {
    /// Decodes one access unit; returns decoded (width, height, NV12, RTP timestamp).
    fn decode(&mut self, data: &[u8], timestamp: u32) -> Result<Vec<Nv12Picture>, String>;
    fn hardware(&self) -> bool;
}

#[cfg(windows)]
struct OsDecoder {
    inner: turbo_pancake_media_video::win::H264Decoder,
}

#[cfg(windows)]
impl OsDecoder {
    fn new() -> Result<Self, String> {
        use turbo_pancake_media_video::win;
        win::startup().map_err(|e| e.to_string())?;
        let (device, _, hardware) = win::create_device().map_err(|e| e.to_string())?;
        // DXVA on a software (WARP) device gains nothing.
        let inner =
            win::H264Decoder::new(hardware.then_some(&device)).map_err(|e| e.to_string())?;
        Ok(Self { inner })
    }
}

#[cfg(windows)]
impl PictureDecoder for OsDecoder {
    fn decode(&mut self, data: &[u8], timestamp: u32) -> Result<Vec<Nv12Picture>, String> {
        self.inner
            .decode(data, i64::from(timestamp))
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        while let Some(f) = self.inner.poll_output() {
            out.push(Nv12Picture {
                width: f.width,
                height: f.height,
                nv12: f.nv12,
                timestamp: f.timestamp as u32,
            });
        }
        Ok(out)
    }

    fn hardware(&self) -> bool {
        self.inner.is_hardware()
    }
}

fn os_decoder() -> Result<Box<dyn PictureDecoder>, String> {
    #[cfg(windows)]
    {
        Ok(Box::new(OsDecoder::new()?))
    }
    #[cfg(not(windows))]
    {
        Err("просмотр трансляций на этой ОС пока не реализован (UNVERIFIED)".into())
    }
}

struct Worker {
    rx: Receiver<VideoIn>,
    slot: PictureSlot,
    keyframes: Arc<Mutex<Option<KeyframeRequester>>>,
    stop: Arc<AtomicBool>,
    reset: Arc<AtomicBool>,
    stats: Arc<Mutex<ViewerStats>>,
}

impl Worker {
    fn run(self) {
        let mut st = ViewerStats::default();
        let mut decoder = match os_decoder() {
            Ok(d) => Some(d),
            Err(e) => {
                st.error = Some(e);
                None
            }
        };
        st.hardware_decoder = decoder.as_ref().is_some_and(|d| d.hardware());
        let mut assembler = FrameAssembler::new();
        let mut rgba = Vec::new();
        let mut serial = 0u64;
        let mut last_request: Option<Instant> = None;
        let mut last_keyframe = Instant::now();
        let mut last_publish = Instant::now();
        let (mut decode_ms, mut latency_ms) = (Vec::new(), Vec::new());
        let mut window_shown = 0u64;
        while !self.stop.load(Ordering::Relaxed) {
            if self.reset.swap(false, Ordering::Relaxed) {
                assembler.reset();
                last_keyframe = Instant::now();
                if let Ok(mut s) = self.slot.lock() {
                    *s = None;
                }
                // A new stream may need its own decoder state (size, SPS).
                if decoder.is_some() {
                    decoder = os_decoder().ok();
                }
            }
            let mut frames = Vec::new();
            match self.rx.recv_timeout(Duration::from_millis(5)) {
                Ok(p) => {
                    st.packets += 1;
                    frames.extend(assembler.push(p));
                    // Take everything queued before decoding (bursts of a keyframe).
                    while let Ok(p) = self.rx.try_recv() {
                        st.packets += 1;
                        frames.extend(assembler.push(p));
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            frames.extend(assembler.poll(Instant::now()));
            let mut want_keyframe = assembler.take_keyframe_request();
            // Every frame is decoded (later ones reference it); only the newest one of
            // this round is converted and shown, so a slow round never builds a backlog.
            let mut newest: Option<Nv12Picture> = None;
            for f in frames {
                st.frames_assembled += 1;
                if f.keyframe {
                    last_keyframe = Instant::now();
                }
                let Some(dec) = decoder.as_mut() else {
                    continue;
                };
                let t0 = Instant::now();
                match dec.decode(&f.data, f.timestamp) {
                    Ok(pictures) => {
                        decode_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
                        st.frames_decoded += pictures.len() as u64;
                        if let Some(p) = pictures.into_iter().last() {
                            newest = Some(p);
                        }
                    }
                    Err(e) => {
                        st.decode_errors += 1;
                        st.error = Some(format!("декодер: {e}"));
                        assembler.reset();
                        want_keyframe = true;
                    }
                }
            }
            if let Some(Nv12Picture {
                width: w,
                height: h,
                nv12,
                timestamp: ts,
            }) = newest
            {
                turbo_pancake_media_video::nv12_to_rgba(w, h, &nv12, &mut rgba);
                serial += 1;
                let late = now_rtp().wrapping_sub(ts) as i32;
                if (0..90_000 * 10).contains(&late) {
                    latency_ms.push(f64::from(late) / 90.0);
                }
                st.frames_shown += 1;
                window_shown += 1;
                (st.width, st.height) = (w, h);
                if let Ok(mut s) = self.slot.lock() {
                    // Reuse the buffer the UI did not take.
                    let buf = match s.take() {
                        Some(old) if old.rgba.len() == rgba.len() => {
                            let mut b = old.rgba;
                            b.copy_from_slice(&rgba);
                            b
                        }
                        _ => rgba.clone(),
                    };
                    *s = Some(Picture {
                        width: w,
                        height: h,
                        rgba: buf,
                        serial,
                    });
                }
            }
            // Packets but no keyframe for a while (joined mid-stream, lost the first one).
            if st.packets > 0
                && last_keyframe.elapsed() > Duration::from_secs(2)
                && st.frames_shown == 0
            {
                want_keyframe = true;
                last_keyframe = Instant::now();
            }
            if want_keyframe
                && last_request.is_none_or(|t| t.elapsed() >= KEYFRAME_REQUEST_INTERVAL)
            {
                if let Some(k) = self.keyframes.lock().ok().and_then(|k| k.clone()) {
                    k.request();
                    st.keyframe_requests += 1;
                }
                last_request = Some(Instant::now());
            }
            if last_publish.elapsed() >= Duration::from_millis(500) {
                st.fps = window_shown as f64 / last_publish.elapsed().as_secs_f64();
                st.frames_dropped = assembler.stats.frames_dropped;
                st.decode_ms_p50 = percentile(&mut decode_ms, 0.5);
                st.latency_ms_p50 = percentile(&mut latency_ms, 0.5);
                st.latency_ms_p95 = percentile(&mut latency_ms, 0.95);
                decode_ms.clear();
                latency_ms.clear();
                window_shown = 0;
                last_publish = Instant::now();
                if let Ok(mut s) = self.stats.lock() {
                    s.clone_from(&st);
                }
            }
        }
    }
}

// ---- Previews (S4) ----
//
// The low layers of the shares shown as thumbnails, decoded on one thread: one assembler
// and one OS decoder per stream (no DXVA: up to 50 small pictures are cheaper on the CPU
// than 50 hardware decoder sessions), newest picture per stream into `PreviewSlots`.

/// Newest preview picture per stream (SSRC); the UI takes them out.
pub type PreviewSlots = Arc<Mutex<std::collections::HashMap<u32, Picture>>>;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PreviewStats {
    /// Streams being decoded.
    pub streams: usize,
    pub packets: u64,
    pub frames_decoded: u64,
    pub frames_dropped: u64,
    pub decode_errors: u64,
    pub keyframe_requests: u64,
    /// Decoded preview pictures per second over all streams (last ≈ 0.5 s).
    pub fps: f64,
    pub decode_ms_p50: f64,
    /// Share of one core the decoder thread spent decoding and converting (last ≈ 0.5 s).
    pub busy: f64,
}

enum PreviewCmd {
    /// The streams to decode now (others are dropped with their decoders and pictures).
    Streams(Vec<(u32, Option<KeyframeRequester>)>),
}

pub struct PreviewDecoder {
    tx: SyncSender<VideoIn>,
    cmds: SyncSender<PreviewCmd>,
    stop: Arc<AtomicBool>,
    stats: Arc<Mutex<PreviewStats>>,
    thread: Option<JoinHandle<()>>,
}

impl PreviewDecoder {
    pub fn start(slots: PreviewSlots) -> std::io::Result<Self> {
        let (tx, rx) = sync_channel(VIEWER_QUEUE);
        let (cmds, cmd_rx) = sync_channel(16);
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Mutex::new(PreviewStats::default()));
        let (st, s) = (stats.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("tp-previews".into())
            .spawn(move || preview_worker(rx, cmd_rx, slots, s, st))?;
        Ok(Self {
            tx,
            cmds,
            stop,
            stats,
            thread: Some(thread),
        })
    }

    /// Where the call thread puts preview packets (`try_send`: full = lost).
    pub fn sender(&self) -> SyncSender<VideoIn> {
        self.tx.clone()
    }

    pub fn set_streams(&self, streams: Vec<(u32, Option<KeyframeRequester>)>) {
        let _ = self.cmds.try_send(PreviewCmd::Streams(streams));
    }

    pub fn stats(&self) -> PreviewStats {
        self.stats.lock().map(|s| s.clone()).unwrap_or_default()
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

impl Drop for PreviewDecoder {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct PreviewStream {
    assembler: FrameAssembler,
    decoder: Option<Box<dyn PictureDecoder>>,
    keyframes: Option<KeyframeRequester>,
    last_request: Option<Instant>,
    started: Instant,
    shown: u64,
    serial: u64,
}

fn preview_decoder() -> Result<Box<dyn PictureDecoder>, String> {
    #[cfg(windows)]
    {
        use turbo_pancake_media_video::win;
        win::startup().map_err(|e| e.to_string())?;
        let inner = win::H264Decoder::new(None).map_err(|e| e.to_string())?;
        Ok(Box::new(OsDecoder { inner }))
    }
    #[cfg(not(windows))]
    {
        Err("просмотр трансляций на этой ОС пока не реализован (UNVERIFIED)".into())
    }
}

fn preview_worker(
    rx: Receiver<VideoIn>,
    cmds: Receiver<PreviewCmd>,
    slots: PreviewSlots,
    stop: Arc<AtomicBool>,
    stats: Arc<Mutex<PreviewStats>>,
) {
    use std::collections::HashMap;
    let mut streams: HashMap<u32, PreviewStream> = HashMap::new();
    let mut st = PreviewStats::default();
    let mut rgba = Vec::new();
    let mut decode_ms = Vec::new();
    let (mut window_frames, mut window_busy) = (0u64, Duration::ZERO);
    let mut last_publish = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        while let Ok(PreviewCmd::Streams(list)) = cmds.try_recv() {
            let keep: HashMap<u32, Option<KeyframeRequester>> = list.into_iter().collect();
            streams.retain(|ssrc, _| keep.contains_key(ssrc));
            if let Ok(mut s) = slots.lock() {
                s.retain(|ssrc, _| keep.contains_key(ssrc));
            }
            for (ssrc, k) in keep {
                let s = streams.entry(ssrc).or_insert_with(|| PreviewStream {
                    assembler: FrameAssembler::new(),
                    decoder: None,
                    keyframes: None,
                    last_request: None,
                    started: Instant::now(),
                    shown: 0,
                    serial: 0,
                });
                s.keyframes = k;
            }
        }
        let mut frames: Vec<(u32, turbo_pancake_media_transport::video::AssembledFrame)> =
            Vec::new();
        match rx.recv_timeout(Duration::from_millis(5)) {
            Ok(p) => {
                let mut push = |p: VideoIn| {
                    st.packets += 1;
                    if let Some(s) = streams.get_mut(&p.ssrc) {
                        let ssrc = p.ssrc;
                        frames.extend(s.assembler.push(p).into_iter().map(|f| (ssrc, f)));
                    }
                };
                push(p);
                while let Ok(p) = rx.try_recv() {
                    push(p);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let now = Instant::now();
        for (ssrc, s) in streams.iter_mut() {
            frames.extend(s.assembler.poll(now).into_iter().map(|f| (*ssrc, f)));
        }
        let t_round = Instant::now();
        let mut newest: std::collections::HashMap<u32, Nv12Picture> = HashMap::new();
        for (ssrc, f) in frames {
            let Some(s) = streams.get_mut(&ssrc) else {
                continue;
            };
            if s.decoder.is_none() {
                match preview_decoder() {
                    Ok(d) => s.decoder = Some(d),
                    Err(_) => continue,
                }
            }
            let Some(dec) = s.decoder.as_mut() else {
                continue;
            };
            let t0 = Instant::now();
            match dec.decode(&f.data, f.timestamp) {
                Ok(pictures) => {
                    decode_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
                    st.frames_decoded += pictures.len() as u64;
                    window_frames += pictures.len() as u64;
                    if let Some(p) = pictures.into_iter().last() {
                        newest.insert(ssrc, p);
                    }
                }
                Err(_) => {
                    st.decode_errors += 1;
                    s.assembler.reset();
                    s.decoder = None;
                }
            }
        }
        for (ssrc, p) in newest {
            let Some(s) = streams.get_mut(&ssrc) else {
                continue;
            };
            turbo_pancake_media_video::nv12_to_rgba(p.width, p.height, &p.nv12, &mut rgba);
            s.serial += 1;
            s.shown += 1;
            if let Ok(mut slots) = slots.lock() {
                slots.insert(
                    ssrc,
                    Picture {
                        width: p.width,
                        height: p.height,
                        rgba: rgba.clone(),
                        serial: s.serial,
                    },
                );
            }
        }
        window_busy += t_round.elapsed();
        // Keyframes: after a loss, or when nothing decodable arrived for a while.
        for s in streams.values_mut() {
            let mut want = s.assembler.take_keyframe_request();
            if s.shown == 0 && s.started.elapsed() > Duration::from_secs(2) {
                want = true;
                s.started = Instant::now();
            }
            if want
                && s.last_request
                    .is_none_or(|t| t.elapsed() >= KEYFRAME_REQUEST_INTERVAL)
            {
                if let Some(k) = &s.keyframes {
                    k.request();
                    st.keyframe_requests += 1;
                }
                s.last_request = Some(Instant::now());
            }
        }
        if last_publish.elapsed() >= Duration::from_millis(500) {
            let secs = last_publish.elapsed().as_secs_f64();
            st.streams = streams.len();
            st.fps = window_frames as f64 / secs;
            st.busy = window_busy.as_secs_f64() / secs;
            st.frames_dropped = streams
                .values()
                .map(|s| s.assembler.stats.frames_dropped)
                .sum();
            st.decode_ms_p50 = percentile(&mut decode_ms, 0.5);
            decode_ms.clear();
            (window_frames, window_busy) = (0, Duration::ZERO);
            last_publish = Instant::now();
            if let Ok(mut s) = stats.lock() {
                s.clone_from(&st);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtp_time_is_90_khz() {
        assert_eq!(rtp_time(10_000_000), 90_000);
        assert_eq!(rtp_time(0), 0);
        // Wraps like an RTP timestamp: 2^32 · 1000 × 9/1000 = 9 · 2^32 ≡ 0.
        assert_eq!(rtp_time((1i64 << 32) * 1000), 0);
    }
}
