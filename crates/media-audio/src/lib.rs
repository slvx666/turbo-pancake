//! Voice pipeline for 1:1 and group calls (spec §4).
//!
//! Layout of the real-time path:
//!
//! ```text
//! mic ─cpal callback─▶ SPSC ring ─▶ worker: resample → 20 ms frame → Opus ─▶ network
//! network ─▶ worker: jitter buffer → Opus decode/FEC/PLC → resample ─▶ SPSC ring ─cpal callback─▶ speaker
//! ```
//!
//! The cpal callbacks ([`device`]) only copy samples between the device buffer and a
//! lock-free ring (no locks, no allocation). Everything that can take noticeable time
//! (resampling, codec, jitter buffer) runs on a worker thread through [`VoiceSender`] and
//! [`VoiceReceiver`], which are plain synchronous types so they can be tested with
//! synthetic signals and simulated networks without any audio hardware. [`AudioEngine`]
//! runs them on a worker thread between the device rings and the transport.

pub mod codec;
pub mod device;
pub mod engine;
pub mod jitter;
pub mod pipeline;
pub mod resample;
pub mod sim;
pub mod testsignal;

pub use codec::{DecodeKind, EncoderConfig, VoiceDecoder, VoiceEncoder};
pub use engine::{
    AudioEngine, AudioIo, CpalIo, EngineConfig, EngineStatus, IncomingPacket, OutgoingFrame,
    PacketSink, PeerId, VoiceLink,
};
pub use jitter::{JitterBuffer, JitterConfig, JitterStats, Playout};
pub use pipeline::{EncodedFrame, ReceiverStats, VoiceReceiver, VoiceSender};
pub use resample::MonoResampler;

/// Opus/network sample rate. Devices running at other rates are resampled.
pub const CODEC_RATE: u32 = 48_000;
/// Duration of one codec frame in milliseconds.
pub const FRAME_MS: u32 = 20;
/// Samples per channel in one 20 ms frame at [`CODEC_RATE`].
pub const FRAME_SAMPLES: usize = (CODEC_RATE / 1000 * FRAME_MS) as usize;
/// Largest Opus packet we produce (RFC 6716 allows 1275 bytes per frame).
pub const MAX_PACKET: usize = 1275;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("opus: {0}")]
    Opus(#[from] opus::Error),
    #[error("resampler: {0}")]
    Resampler(String),
    #[error("device: {0}")]
    Device(String),
    #[error("no such device: {0}")]
    NoDevice(String),
    #[error("audio engine: {0}")]
    Engine(String),
}
