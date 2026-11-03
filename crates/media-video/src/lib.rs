//! Screen-share video (spec §5, S3): H.264 encode and decode through the operating system's
//! codecs only (ADR 0009) — Media Foundation on Windows. macOS (VideoToolbox) and Linux
//! (VA-API) report [`VideoError::Unsupported`] until they are implemented and verified.
//!
//! The encoder and decoder are plain synchronous objects owned by one thread each; the
//! caller drives them (`encode` / `poll_output`) from its own worker loop. The one thread
//! this crate starts is the sending pipeline of a share ([`ScreenShare`]: capture →
//! convert → encode at a fixed rate, newest frame wins, no growing queues).

#[cfg(windows)]
pub mod win;

#[cfg(windows)]
pub use win::{ScreenShare, monitors};

/// A monitor that can be shared (spec §5.1: whole monitors only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorInfo {
    pub index: usize,
    pub name: String,
    /// Position on the virtual desktop.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub primary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareSource {
    /// Monitor by index in [`monitors`].
    Monitor(usize),
    /// A generated moving picture (tests and trials without a screen).
    Synthetic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShareConfig {
    pub source: ShareSource,
    /// Encoded size (the monitor is scaled to it on the GPU) and frame rate.
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub require_hardware: bool,
}

impl ShareConfig {
    /// 1080p60 at 8 Mbit/s (spec §5.1 S3 mode).
    pub fn hd60(source: ShareSource) -> Self {
        Self {
            source,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 8000,
            require_hardware: false,
        }
    }

    /// 720p30 at 2.5 Mbit/s (spec §5.1 lower mode).
    pub fn hd30(source: ShareSource) -> Self {
        Self {
            source,
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: 2500,
            require_hardware: false,
        }
    }
}

/// Sending side counters; rates and percentiles cover the last ≈ 0.5 s.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShareStats {
    pub source_width: u32,
    pub source_height: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub encoder: String,
    pub hardware: bool,
    pub adapter: String,
    /// Frames the capture delivered (only when the screen content changes).
    pub captured_frames: u64,
    pub submitted_frames: u64,
    /// Submitted frames that repeat the previous picture (static screen).
    pub submitted_repeat: u64,
    pub encoded_frames: u64,
    pub keyframes: u64,
    pub keyframe_requests: u64,
    pub bytes: u64,
    /// Ticks without encoder input capacity (frame skipped).
    pub dropped_encoder_busy: u64,
    /// Encoded frames the transport could not take.
    pub dropped_delivery: u64,
    pub ticks_skipped: u64,
    pub captured_fps: f64,
    pub encoded_fps: f64,
    pub bitrate_kbps: f64,
    pub bitrate_kbps_target: u32,
    pub encode_ms_p50: f64,
    pub encode_ms_p95: f64,
    pub capture_to_encoded_ms_p50: f64,
    pub capture_to_encoded_ms_p95: f64,
    /// Why the pipeline stopped on its own (resolution change, device lost, …).
    pub error: Option<String>,
}

/// p-quantile of `values` (sorted in place); 0 when empty.
pub fn percentile(values: &mut [f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[((values.len() - 1) as f64 * p).round() as usize]
}

/// A test picture in NV12: a diagonal gradient and a bright square moving with `n`.
pub fn synthetic_nv12(width: u32, height: u32, n: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut out = vec![128u8; w * h * 3 / 2];
    let side = (h / 4).max(2);
    let x0 = (n as usize * 8) % (w.saturating_sub(side).max(1));
    let y0 = (n as usize * 4) % (h.saturating_sub(side).max(1));
    for y in 0..h {
        let row = &mut out[y * w..(y + 1) * w];
        for (x, px) in row.iter_mut().enumerate() {
            let inside = (x0..x0 + side).contains(&x) && (y0..y0 + side).contains(&y);
            *px = if inside {
                235
            } else {
                (16 + (x + y) * 160 / (w + h)) as u8
            };
        }
    }
    out
}

/// NV12 (BT.709, limited range, tight rows, even size) → RGBA8 for display, in bands of
/// rows on up to 4 threads (≈ 9 ms for 1080p on one core of the dev machine).
pub fn nv12_to_rgba(width: u32, height: u32, nv12: &[u8], out: &mut Vec<u8>) {
    let (w, h) = (width as usize, height as usize);
    out.resize(w * h * 4, 255);
    if nv12.len() < w * h * 3 / 2 || w < 2 || h < 2 {
        return;
    }
    let (luma, chroma) = nv12.split_at(w * h);
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .clamp(1, 4);
    // Bands of whole row pairs (one chroma row each).
    let pairs_per_band = (h / 2).div_ceil(threads);
    let rows = pairs_per_band * 2;
    std::thread::scope(|scope| {
        for (band, dst) in out.chunks_mut(rows * w * 4).enumerate() {
            let first = band * rows;
            scope.spawn(move || {
                convert_rows(w, first, &luma[first * w..], chroma, dst);
            });
        }
    });
}

/// Converts the rows of `dst` (starting at picture row `first`).
fn convert_rows(w: usize, first: usize, luma: &[u8], chroma: &[u8], dst: &mut [u8]) {
    let px = |c: i32| (c >> 8).clamp(0, 255) as u8;
    for (i, (y_row, dst)) in luma
        .chunks_exact(w)
        .zip(dst.chunks_exact_mut(w * 4))
        .enumerate()
    {
        let y = first + i;
        let uv_row = &chroma[(y / 2) * w..(y / 2) * w + w];
        for ((ys, uv), d) in y_row
            .as_chunks::<2>()
            .0
            .iter()
            .zip(uv_row.as_chunks::<2>().0)
            .zip(dst.as_chunks_mut::<8>().0)
        {
            let cb = i32::from(uv[0]) - 128;
            let cr = i32::from(uv[1]) - 128;
            // BT.709 limited range in 8-bit fixed point (luma scale 298/256).
            let r = 459 * cr + 128;
            let g = -55 * cb - 136 * cr + 128;
            let b = 541 * cb + 128;
            let c0 = (i32::from(ys[0]) - 16) * 298;
            let c1 = (i32::from(ys[1]) - 16) * 298;
            d[0] = px(c0 + r);
            d[1] = px(c0 + g);
            d[2] = px(c0 + b);
            d[3] = 255;
            d[4] = px(c1 + r);
            d[5] = px(c1 + g);
            d[6] = px(c1 + b);
            d[7] = 255;
        }
    }
}

/// Monitors that can be shared; empty where capture is not implemented yet.
#[cfg(not(windows))]
pub fn monitors() -> Vec<MonitorInfo> {
    Vec::new()
}

/// Placeholder where the sending pipeline is not implemented yet (macOS: ScreenCaptureKit
/// + VideoToolbox, Linux: portal/PipeWire + VA-API).
#[cfg(not(windows))]
pub struct ScreenShare;

#[cfg(not(windows))]
impl ScreenShare {
    pub fn start(
        _config: ShareConfig,
        _deliver: Box<dyn FnMut(EncodedFrame) -> bool + Send>,
    ) -> Result<Self, VideoError> {
        Err(VideoError::Unsupported)
    }
    pub fn request_keyframe(&self) {}
    pub fn set_bitrate(&self, _kbps: u32) {}
    pub fn stats(&self) -> ShareStats {
        ShareStats::default()
    }
    pub fn is_running(&self) -> bool {
        false
    }
    pub fn stop(self) {}
}

/// Encoder settings. `width`/`height` must be even.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Keyframe interval in frames (0 = only on request).
    pub gop_frames: u32,
    /// Only accept a hardware encoder (no OS software fallback).
    pub require_hardware: bool,
    /// Skip hardware encoders (tests of the software fallback, troubleshooting).
    pub software_only: bool,
}

impl EncoderConfig {
    pub fn new(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Self {
        Self {
            width,
            height,
            fps,
            bitrate_kbps,
            gop_frames: fps * 2,
            require_hardware: false,
            software_only: false,
        }
    }

    pub fn validate(&self) -> Result<(), VideoError> {
        if self.width == 0
            || self.height == 0
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
        {
            return Err(VideoError::InvalidConfig("size must be non-zero and even"));
        }
        if self.fps == 0 || self.fps > 240 {
            return Err(VideoError::InvalidConfig("fps must be 1..=240"));
        }
        if self.require_hardware && self.software_only {
            return Err(VideoError::InvalidConfig(
                "hardware required but software only",
            ));
        }
        if self.bitrate_kbps == 0 {
            return Err(VideoError::InvalidConfig("bitrate must be > 0"));
        }
        Ok(())
    }
}

/// One H.264 access unit in Annex B format (start codes), as produced by the encoder.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    /// Presentation time in 100 ns units, as given to `encode`.
    pub timestamp: i64,
    pub keyframe: bool,
}

/// A decoded picture in NV12 with tight rows (stride = width).
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub timestamp: i64,
    /// `width * height` luma bytes followed by `width * height / 2` interleaved UV bytes.
    pub nv12: Vec<u8>,
}

impl DecodedFrame {
    pub fn luma(&self) -> &[u8] {
        &self.nv12[..(self.width * self.height) as usize]
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VideoError {
    #[error("invalid configuration: {0}")]
    InvalidConfig(&'static str),
    #[error("no suitable H.264 {0} on this system")]
    NoCodec(&'static str),
    #[error("not supported on this platform yet")]
    Unsupported,
    #[error("{context}: {message}")]
    Os {
        context: &'static str,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_to_rgba_reproduces_grey_and_primaries() {
        // 2×2 picture: luma 16 (black) / 235 (white), neutral chroma.
        let nv12 = [16, 235, 235, 16, 128, 128];
        let mut out = Vec::new();
        nv12_to_rgba(2, 2, &nv12, &mut out);
        assert_eq!(&out[0..4], &[0, 0, 0, 255]);
        assert_eq!(&out[4..8], &[255, 255, 255, 255]);
        // Red in BT.709 limited range: Y 63, Cb 102, Cr 240.
        let red = [63, 63, 63, 63, 102, 240];
        nv12_to_rgba(2, 2, &red, &mut out);
        assert!(
            out[0] > 240 && out[1] < 15 && out[2] < 15,
            "{:?}",
            &out[..4]
        );
        let pic = synthetic_nv12(64, 36, 3);
        assert_eq!(pic.len(), 64 * 36 * 3 / 2);
        let mut v = vec![3.0, 1.0, 2.0];
        assert_eq!(percentile(&mut v, 0.5), 2.0);
        assert_eq!(percentile(&mut [], 0.5), 0.0);
    }

    #[test]
    fn config_validation() {
        assert!(EncoderConfig::new(1920, 1080, 60, 8000).validate().is_ok());
        assert!(EncoderConfig::new(1921, 1080, 60, 8000).validate().is_err());
        assert!(EncoderConfig::new(1920, 1080, 0, 8000).validate().is_err());
        assert!(EncoderConfig::new(1920, 1080, 60, 0).validate().is_err());
        assert_eq!(EncoderConfig::new(1280, 720, 30, 2000).gop_frames, 60);
    }
}
