//! Hardware-independent send and receive halves of a voice stream.
//!
//! [`VoiceSender`]: device-rate mono samples → 48 kHz → 20 ms frames → Opus packets with
//! RTP-style sequence numbers and timestamps.
//! [`VoiceReceiver`]: packets → jitter buffer → Opus decode / FEC / PLC → device rate.

use crate::codec::{DecodeKind, EncoderConfig, VoiceDecoder, VoiceEncoder};
use crate::jitter::{JitterBuffer, JitterConfig, JitterStats, Playout};
use crate::resample::MonoResampler;
use crate::{AudioError, CODEC_RATE, FRAME_SAMPLES, MAX_PACKET};

/// One encoded 20 ms frame, borrowed from the sender's buffer.
#[derive(Debug)]
pub struct EncodedFrame<'a> {
    pub seq: u16,
    /// RTP timestamp in 48 kHz units.
    pub timestamp: u32,
    pub payload: &'a [u8],
    /// Peak level of the frame before encoding (0 when muted), for activity display.
    pub peak: f32,
}

pub struct VoiceSender {
    resampler: MonoResampler,
    encoder: VoiceEncoder,
    resampled: Vec<f32>,
    frame: Vec<f32>,
    packet: Vec<u8>,
    seq: u16,
    timestamp: u32,
    muted: bool,
    frames: u64,
}

impl VoiceSender {
    pub fn new(input_rate: u32, config: EncoderConfig) -> Result<Self, AudioError> {
        Ok(Self {
            resampler: MonoResampler::new(input_rate, CODEC_RATE)?,
            encoder: VoiceEncoder::new(config)?,
            resampled: Vec::with_capacity(FRAME_SAMPLES * 4),
            frame: Vec::with_capacity(FRAME_SAMPLES),
            packet: vec![0; MAX_PACKET],
            seq: 0,
            timestamp: 0,
            muted: false,
            frames: 0,
        })
    }

    /// Muted frames are still sent (as encoded digital silence) so the receiver's
    /// clock and jitter estimate keep running; nothing from the microphone leaves.
    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }

    pub fn muted(&self) -> bool {
        self.muted
    }

    /// Switches to a microphone running at another rate (device change). Sequence
    /// numbers and timestamps continue, so the receiver sees one uninterrupted stream;
    /// samples buffered from the old device are dropped.
    pub fn set_input_rate(&mut self, input_rate: u32) -> Result<(), AudioError> {
        self.resampler = MonoResampler::new(input_rate, CODEC_RATE)?;
        self.resampled.clear();
        Ok(())
    }

    pub fn input_rate(&self) -> u32 {
        self.resampler.rate_in()
    }

    pub fn encoder(&mut self) -> &mut VoiceEncoder {
        &mut self.encoder
    }

    pub fn frames_sent(&self) -> u64 {
        self.frames
    }

    /// Consumes mono samples at the device rate and calls `emit` for every complete frame.
    pub fn push_samples(
        &mut self,
        mono: &[f32],
        mut emit: impl FnMut(EncodedFrame<'_>),
    ) -> Result<(), AudioError> {
        self.resampler.process(mono, &mut self.resampled)?;
        let mut used = 0;
        while self.resampled.len() - used >= FRAME_SAMPLES - self.frame.len() {
            let take = FRAME_SAMPLES - self.frame.len();
            self.frame
                .extend_from_slice(&self.resampled[used..used + take]);
            used += take;
            let peak = if self.muted {
                self.frame.fill(0.0);
                0.0
            } else {
                self.frame.iter().fold(0.0f32, |m, v| m.max(v.abs()))
            };
            let n = self.encoder.encode(&self.frame, &mut self.packet)?;
            emit(EncodedFrame {
                seq: self.seq,
                timestamp: self.timestamp,
                payload: &self.packet[..n],
                peak,
            });
            self.seq = self.seq.wrapping_add(1);
            self.timestamp = self.timestamp.wrapping_add(FRAME_SAMPLES as u32);
            self.frames += 1;
            self.frame.clear();
        }
        // Keep the remainder (shorter than a frame) for the next call.
        self.frame.extend_from_slice(&self.resampled[used..]);
        self.resampled.clear();
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ReceiverStats {
    pub jitter: JitterStats,
    /// 20 ms ticks rendered (including buffering silence).
    pub ticks: u64,
    pub buffering_ticks: u64,
    pub decode_errors: u64,
    /// Peak level of the last rendered frame.
    pub last_peak: f32,
}

impl ReceiverStats {
    /// Share of played ticks that had no data for their own frame (FEC or PLC).
    pub fn repaired_ratio(&self) -> f64 {
        let played = self.ticks - self.buffering_ticks;
        if played == 0 {
            0.0
        } else {
            (self.jitter.fec + self.jitter.concealed) as f64 / played as f64
        }
    }

    /// Share of played ticks rendered by concealment only.
    pub fn concealed_ratio(&self) -> f64 {
        let played = self.ticks - self.buffering_ticks;
        if played == 0 {
            0.0
        } else {
            self.jitter.concealed as f64 / played as f64
        }
    }
}

pub struct VoiceReceiver {
    jitter: JitterBuffer,
    decoder: VoiceDecoder,
    resampler: MonoResampler,
    pcm: Vec<f32>,
    volume: f32,
    stats: ReceiverStats,
}

impl VoiceReceiver {
    pub fn new(output_rate: u32, config: JitterConfig) -> Result<Self, AudioError> {
        Ok(Self {
            jitter: JitterBuffer::new(config),
            decoder: VoiceDecoder::new()?,
            resampler: MonoResampler::new(CODEC_RATE, output_rate)?,
            pcm: vec![0.0; FRAME_SAMPLES],
            volume: 1.0,
            stats: ReceiverStats::default(),
        })
    }

    /// Local playback gain for this peer (does not affect what the peer sends).
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 4.0);
    }

    pub fn push_packet(&mut self, seq: u16, payload: Vec<u8>, arrival_ms: u64) {
        self.jitter.insert(seq, payload, arrival_ms);
    }

    /// Frames buffered ahead of the playout point.
    pub fn buffered_frames(&self) -> u64 {
        self.jitter.depth()
    }

    pub fn stats(&self) -> ReceiverStats {
        ReceiverStats {
            jitter: self.jitter.stats(),
            ..self.stats
        }
    }

    /// Renders one 20 ms tick and appends it (at the output rate) to `out`.
    pub fn pull_frame(&mut self, out: &mut Vec<f32>) -> Result<Option<DecodeKind>, AudioError> {
        self.stats.ticks += 1;
        let kind = match self.jitter.pop() {
            Playout::Buffering => {
                self.stats.buffering_ticks += 1;
                self.pcm.fill(0.0);
                None
            }
            Playout::Packet(p) => Some(self.decode(&p, DecodeKind::Normal)),
            Playout::Fec(p) => Some(self.decode(&p, DecodeKind::Fec)),
            Playout::Lost => Some(self.decode(&[], DecodeKind::Concealed)),
        };
        let mut peak = 0.0f32;
        for s in &mut self.pcm {
            *s *= self.volume;
            peak = peak.max(s.abs());
        }
        self.stats.last_peak = peak;
        self.resampler.process(&self.pcm, out)?;
        Ok(kind)
    }

    fn decode(&mut self, packet: &[u8], kind: DecodeKind) -> DecodeKind {
        let res = match kind {
            DecodeKind::Normal => self.decoder.decode(packet, &mut self.pcm),
            DecodeKind::Fec => self.decoder.decode_fec(packet, &mut self.pcm),
            DecodeKind::Concealed => self.decoder.conceal(&mut self.pcm),
        };
        match res {
            Ok(n) if n == FRAME_SAMPLES => kind,
            Ok(_) | Err(_) => {
                // Corrupt or unexpected frame size: render concealment instead.
                self.stats.decode_errors += 1;
                if self.decoder.conceal(&mut self.pcm).is_err() {
                    self.pcm.fill(0.0);
                }
                DecodeKind::Concealed
            }
        }
    }
}
