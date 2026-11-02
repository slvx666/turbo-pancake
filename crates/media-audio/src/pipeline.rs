//! Hardware-independent send and receive halves of a voice stream.
//!
//! [`VoiceSender`]: device-rate mono samples → 48 kHz → 20 ms frames → Opus packets with
//! RTP-style sequence numbers and timestamps. With voice activation, frames below the
//! threshold are not sent once the hangover runs out: the timestamp keeps running while
//! the sequence number pauses, as in RTP, so the receiver sees a pause, not a loss.
//! [`VoiceReceiver`]: packets → jitter buffer → Opus decode / FEC / PLC → device rate.

use crate::codec::{DecodeKind, EncoderConfig, VoiceDecoder, VoiceEncoder};
use crate::jitter::{JitterBuffer, JitterConfig, JitterStats, Playout};
use crate::resample::MonoResampler;
use crate::{AudioError, CODEC_RATE, FRAME_SAMPLES, MAX_PACKET};

/// Speech level for the speaking indicators when voice activation is off, dBFS (RMS of
/// a 20 ms frame).
pub const SPEECH_DB: f32 = -45.0;
/// Frames a voice stays "speaking" after its level drops below the threshold (300 ms),
/// so word gaps neither cut the stream nor flicker the indicator.
pub const HANGOVER_FRAMES: u32 = 15;

/// RMS level of a frame in dBFS (−100 for digital silence).
pub fn level_db(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return -100.0;
    }
    let mean = frame.iter().map(|v| v * v).sum::<f32>() / frame.len() as f32;
    (10.0 * mean.max(1e-10).log10()).max(-100.0)
}

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
    /// Voice activation threshold in dBFS; `None` = always send.
    gate_db: Option<f32>,
    /// Quiet frames left before the hangover ends.
    speech_left: u32,
    speaking: bool,
    gated: u64,
    last_peak: f32,
    last_db: f32,
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
            gate_db: None,
            speech_left: 0,
            speaking: false,
            gated: 0,
            last_peak: 0.0,
            last_db: -100.0,
        })
    }

    /// Voice activation: below `threshold_db` (dBFS) frames stop being sent after the
    /// hangover. `None` sends every frame (muted ones as silence).
    pub fn set_voice_gate(&mut self, threshold_db: Option<f32>) {
        self.gate_db = threshold_db.map(|t| t.clamp(-100.0, 0.0));
    }

    pub fn voice_gate(&self) -> Option<f32> {
        self.gate_db
    }

    /// The user is speaking: level above the gate threshold (or [`SPEECH_DB`] without
    /// voice activation) within the hangover, and not muted.
    pub fn speaking(&self) -> bool {
        self.speaking
    }

    /// Frames held back by voice activation.
    pub fn frames_gated(&self) -> u64 {
        self.gated
    }

    /// Peak and RMS level (dBFS) of the last frame, after processing and mute.
    pub fn last_level(&self) -> (f32, f32) {
        (self.last_peak, self.last_db)
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
        emit: impl FnMut(EncodedFrame<'_>),
    ) -> Result<(), AudioError> {
        self.push_samples_with(mono, |_| Ok(()), emit)
    }

    /// Like [`Self::push_samples`], but runs `process` (echo/noise processing) on every
    /// 48 kHz frame before muting, level metering and encoding.
    pub fn push_samples_with(
        &mut self,
        mono: &[f32],
        mut process: impl FnMut(&mut [f32]) -> Result<(), AudioError>,
        mut emit: impl FnMut(EncodedFrame<'_>),
    ) -> Result<(), AudioError> {
        self.resampler.process(mono, &mut self.resampled)?;
        let mut used = 0;
        while self.resampled.len() - used >= FRAME_SAMPLES - self.frame.len() {
            let take = FRAME_SAMPLES - self.frame.len();
            self.frame
                .extend_from_slice(&self.resampled[used..used + take]);
            used += take;
            // Processed even while muted: AEC3 needs a continuous capture stream next to
            // the render reference, otherwise its render queue overruns and it resets.
            process(&mut self.frame)?;
            let peak = if self.muted {
                self.frame.fill(0.0);
                0.0
            } else {
                self.frame.iter().fold(0.0f32, |m, v| m.max(v.abs()))
            };
            let db = if self.muted {
                -100.0
            } else {
                level_db(&self.frame)
            };
            (self.last_peak, self.last_db) = (peak, db);
            if db >= self.gate_db.unwrap_or(SPEECH_DB) {
                self.speaking = true;
                self.speech_left = HANGOVER_FRAMES;
            } else {
                self.speaking = self.speech_left > 0;
                self.speech_left = self.speech_left.saturating_sub(1);
            }
            if self.gate_db.is_none() || self.speaking {
                let n = self.encoder.encode(&self.frame, &mut self.packet)?;
                emit(EncodedFrame {
                    seq: self.seq,
                    timestamp: self.timestamp,
                    payload: &self.packet[..n],
                    peak,
                });
                self.seq = self.seq.wrapping_add(1);
                self.frames += 1;
            } else {
                self.gated += 1;
            }
            self.timestamp = self.timestamp.wrapping_add(FRAME_SAMPLES as u32);
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
    /// Peak level of the last rendered frame (after the local volume).
    pub last_peak: f32,
    /// Received speech above [`SPEECH_DB`] within the hangover (before the local volume,
    /// so a member turned down locally still shows as speaking).
    pub speaking: bool,
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
    speech_left: u32,
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
            speech_left: 0,
        })
    }

    /// Local playback gain for this peer (does not affect what the peer sends).
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 4.0);
    }

    pub fn push_packet(&mut self, seq: u16, payload: Vec<u8>, arrival_ms: u64) {
        self.jitter.insert(seq, payload, arrival_ms);
    }

    /// Like [`Self::push_packet`] with the RTP timestamp, which shows the sender's pauses
    /// (voice activation).
    pub fn push_packet_at(&mut self, seq: u16, timestamp: u32, payload: Vec<u8>, arrival_ms: u64) {
        self.jitter.insert_at(seq, timestamp, payload, arrival_ms);
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
        // Concealment extrapolates the last sound: it is not speech.
        let heard = matches!(kind, Some(DecodeKind::Normal | DecodeKind::Fec));
        if heard && level_db(&self.pcm) >= SPEECH_DB {
            self.speech_left = HANGOVER_FRAMES;
        } else {
            self.speech_left = self.speech_left.saturating_sub(1);
        }
        self.stats.speaking = self.speech_left > 0;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 0.3-peak 440 Hz tone (about −13.5 dBFS), or silence.
    fn chunk(frames: usize, tone: bool, phase: &mut f32) -> Vec<f32> {
        (0..frames * FRAME_SAMPLES)
            .map(|_| {
                *phase += 440.0 / CODEC_RATE as f32;
                if tone {
                    0.3 * (std::f32::consts::TAU * *phase).sin()
                } else {
                    0.0
                }
            })
            .collect()
    }

    #[test]
    fn voice_gate_pauses_sending_with_hangover_and_keeps_the_clock() {
        let mut tx = VoiceSender::new(CODEC_RATE, EncoderConfig::default()).unwrap();
        tx.set_voice_gate(Some(-40.0));
        let mut phase = 0.0;
        let mut sent: Vec<(u16, u32)> = Vec::new();
        let mut speaking = Vec::new();
        for (frames, tone) in [(25, true), (50, false), (25, true)] {
            for _ in 0..frames {
                tx.push_samples(&chunk(1, tone, &mut phase), |f| {
                    sent.push((f.seq, f.timestamp))
                })
                .unwrap();
                speaking.push(tx.speaking());
            }
        }
        // 25 speech + 15 hangover + 25 speech frames sent; the rest of the pause held back.
        assert_eq!(sent.len(), 25 + HANGOVER_FRAMES as usize + 25);
        assert_eq!(tx.frames_gated(), 50 - u64::from(HANGOVER_FRAMES));
        // Sequence numbers are contiguous, the timestamp jumps over the pause.
        for w in sent.windows(2) {
            assert_eq!(w[1].0, w[0].0.wrapping_add(1));
        }
        let resume = 25 + HANGOVER_FRAMES as usize;
        let gap = sent[resume].1.wrapping_sub(sent[resume - 1].1);
        assert_eq!(gap, (50 - HANGOVER_FRAMES + 1) * FRAME_SAMPLES as u32);
        assert!(speaking[..25].iter().all(|s| *s));
        assert!(!speaking[25 + HANGOVER_FRAMES as usize]);
        assert!(speaking[75..].iter().all(|s| *s));
        assert!(tx.last_level().1 > -20.0);
    }

    #[test]
    fn without_gate_every_frame_is_sent_and_mute_silences_the_indicator() {
        let mut tx = VoiceSender::new(CODEC_RATE, EncoderConfig::default()).unwrap();
        let mut phase = 0.0;
        let mut n = 0;
        tx.push_samples(&chunk(20, true, &mut phase), |_| n += 1)
            .unwrap();
        assert!(tx.speaking());
        tx.set_muted(true);
        tx.push_samples(&chunk(20, true, &mut phase), |_| n += 1)
            .unwrap();
        assert_eq!(n, 40);
        assert_eq!(tx.frames_gated(), 0);
        assert!(!tx.speaking());
        assert_eq!(tx.last_level(), (0.0, -100.0));
        // Muted with voice activation: nothing leaves at all.
        tx.set_voice_gate(Some(-60.0));
        tx.push_samples(&chunk(20, true, &mut phase), |_| n += 1)
            .unwrap();
        assert_eq!(n, 40);
    }

    #[test]
    fn receiver_plays_both_talkspurts_and_shows_speaking() {
        let mut tx = VoiceSender::new(CODEC_RATE, EncoderConfig::default()).unwrap();
        tx.set_voice_gate(Some(-40.0));
        let mut rx = VoiceReceiver::new(CODEC_RATE, JitterConfig::default()).unwrap();
        let mut phase = 0.0;
        let mut out = Vec::new();
        let mut speaking = Vec::new();
        let mut t = 0u64;
        for (frames, tone) in [(50, true), (100, false), (50, true), (10, false)] {
            for _ in 0..frames {
                tx.push_samples(&chunk(1, tone, &mut phase), |f| {
                    rx.push_packet_at(f.seq, f.timestamp, f.payload.to_vec(), t)
                })
                .unwrap();
                out.clear();
                rx.pull_frame(&mut out).unwrap();
                speaking.push(rx.stats().speaking);
                t += 20;
            }
        }
        let s = rx.stats();
        assert_eq!(s.jitter.late, 0, "{s:?}");
        assert_eq!(s.jitter.talkspurts, 1, "{s:?}");
        // Everything received was played, except the last frames still buffered.
        assert_eq!(s.jitter.dropped, 0, "{s:?}");
        assert!(s.jitter.received - s.jitter.played <= 3, "{s:?}");
        assert!(speaking[10..50].iter().all(|x| *x));
        // Silent during the pause (after the hangover and the playout delay).
        assert!(speaking[90..150].iter().all(|x| !*x));
        assert!(speaking[170..200].iter().all(|x| *x));
    }
}
