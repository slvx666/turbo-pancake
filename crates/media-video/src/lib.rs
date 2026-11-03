//! Screen-share video (spec §5, S3): H.264 encode and decode through the operating system's
//! codecs only (ADR 0009) — Media Foundation on Windows. macOS (VideoToolbox) and Linux
//! (VA-API) report [`VideoError::Unsupported`] until they are implemented and verified.
//!
//! The encoder and decoder are plain synchronous objects owned by one thread each; the
//! caller drives them (`encode` / `poll_output`) from its own worker loop, so nothing here
//! starts threads or grows queues behind the caller's back.

#[cfg(windows)]
pub mod win;

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
    fn config_validation() {
        assert!(EncoderConfig::new(1920, 1080, 60, 8000).validate().is_ok());
        assert!(EncoderConfig::new(1921, 1080, 60, 8000).validate().is_err());
        assert!(EncoderConfig::new(1920, 1080, 0, 8000).validate().is_err());
        assert!(EncoderConfig::new(1920, 1080, 60, 0).validate().is_err());
        assert_eq!(EncoderConfig::new(1280, 720, 30, 2000).gop_frames, 60);
    }
}
