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

/// Below this level (dBFS) a frame removed to shorten the delay is simply left out.
const SKIP_SILENT_DB: f32 = -55.0;

/// Removing sound to shorten the delay: one stretch of `lag` samples is taken out of
/// the queued signal by fading over `fade` samples from the signal into itself `lag`
/// samples later. With `lag` = one pitch period of a voiced sound both sides of the
/// fade are the same waveform, so nothing is heard; noise and silence hide any fade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cut {
    lag: usize,
    fade: usize,
}

impl Cut {
    /// Pitch periods looked for: 2 ms (500 Hz) to 15 ms (67 Hz).
    const MIN_LAG: usize = (CODEC_RATE / 500) as usize;
    const MAX_LAG: usize = (CODEC_RATE / 1000 * 15) as usize;
    /// Samples compared (and faded): 8 ms.
    const WINDOW: usize = (CODEC_RATE / 125) as usize;
    /// What is removed at once where no period is found (noise, silence): 5 ms.
    const PLAIN: usize = (CODEC_RATE / 200) as usize;

    /// The cut that removes at most `excess` samples from the front part of `q`, or
    /// `None` when there is nothing to remove (or not enough signal to fade over).
    fn find(q: &[f32], excess: usize) -> Option<Cut> {
        if excess == 0 || q.len() < Self::WINDOW * 2 {
            return None;
        }
        let fade = Self::WINDOW;
        let room = q.len() - fade;
        let max_lag = excess.min(Self::MAX_LAG).min(room);
        // What is left is shorter than any pitch period: take it out as it is.
        if max_lag < Self::MIN_LAG {
            return Some(Cut {
                lag: excess.min(room),
                fade,
            });
        }
        let head = &q[..fade];
        let energy: f32 = head.iter().map(|v| v * v).sum();
        let plain = Cut {
            lag: excess.min(Self::PLAIN).min(room),
            fade,
        };
        if energy < 1e-6 * fade as f32 {
            return Some(plain);
        }
        let mut best = (0.0f32, 0usize);
        for lag in Self::MIN_LAG..=max_lag {
            let later = &q[lag..lag + fade];
            let (mut dot, mut e2) = (0.0f32, 0.0f32);
            for (a, b) in head.iter().zip(later) {
                dot += a * b;
                e2 += b * b;
            }
            let c = dot / (energy * e2).sqrt().max(1e-9);
            if c > best.0 {
                best = (c, lag);
            }
        }
        // Voiced: the waveform repeats after `lag`. Otherwise it is noise-like.
        Some(if best.0 >= 0.6 {
            Cut { lag: best.1, fade }
        } else {
            plain
        })
    }

    fn apply(self, q: &mut Vec<f32>) {
        if q.len() < self.lag + self.fade {
            return;
        }
        for i in 0..self.fade {
            let w = i as f32 / self.fade as f32;
            q[i] = q[i] * (1.0 - w) + q[i + self.lag] * w;
        }
        q.drain(self.fade..self.fade + self.lag);
    }
}

pub struct VoiceReceiver {
    jitter: JitterBuffer,
    decoder: VoiceDecoder,
    resampler: MonoResampler,
    pcm: Vec<f32>,
    /// Sound decoded but not played yet while the delay is being shortened: after two
    /// frames arrived for one tick, one frame's worth waits here and is taken out a
    /// pitch period per tick ([`Cut`]). Empty otherwise.
    queue: Vec<f32>,
    /// Stereo stream: the decoded interleaved frame, and the side signal (L − R) / 2 of
    /// the current tick and of the first of two merged frames. `pcm` then holds the mid
    /// signal (L + R) / 2 — exactly what a mono decoder plays.
    stereo: Option<Stereo>,
    volume: f32,
    stats: ReceiverStats,
    speech_left: u32,
}

struct Stereo {
    wide: Vec<f32>,
    side: Vec<f32>,
    /// The side signal's part of [`VoiceReceiver::queue`] (cut at the same places).
    queue: Vec<f32>,
}

impl VoiceReceiver {
    pub fn new(output_rate: u32, config: JitterConfig) -> Result<Self, AudioError> {
        Self::with_channels(output_rate, config, false)
    }

    /// `stereo`: the stream is stereo (a share's system audio). The frames from
    /// [`Self::pull_frame`] are then its mid signal and [`Self::side`] its side signal,
    /// so `left = mid + side`, `right = mid − side`; a mono listener plays the mid alone.
    pub fn with_channels(
        output_rate: u32,
        config: JitterConfig,
        stereo: bool,
    ) -> Result<Self, AudioError> {
        Ok(Self {
            jitter: JitterBuffer::new(config),
            decoder: VoiceDecoder::with_channels(stereo)?,
            resampler: MonoResampler::new(CODEC_RATE, output_rate)?,
            pcm: vec![0.0; FRAME_SAMPLES],
            queue: Vec::with_capacity(FRAME_SAMPLES * 3),
            stereo: stereo.then(|| Stereo {
                wide: vec![0.0; FRAME_SAMPLES * 2],
                side: vec![0.0; FRAME_SAMPLES],
                queue: Vec::with_capacity(FRAME_SAMPLES * 3),
            }),
            volume: 1.0,
            stats: ReceiverStats::default(),
            speech_left: 0,
        })
    }

    pub fn is_stereo(&self) -> bool {
        self.stereo.is_some()
    }

    /// Samples decoded but held back while the delay is being shortened (0 normally).
    pub fn pending_samples(&self) -> usize {
        self.queue.len()
    }

    /// The side signal of the tick last rendered by [`Self::pull_frame`], at
    /// [`CODEC_RATE`] and after the local volume; `None` for a mono stream.
    pub fn side(&self) -> Option<&[f32]> {
        self.stereo.as_ref().map(|s| s.side.as_slice())
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
                if let Some(st) = &mut self.stereo {
                    st.side.fill(0.0);
                }
                None
            }
            Playout::Packet(p) => Some(self.decode(&p, DecodeKind::Normal)),
            Playout::Merge(a, b) => {
                // Two frames for one tick: the buffer is too deep. Both are decoded
                // (the decoder's state must see every packet). A silent first frame is
                // simply left out; otherwise it waits in front of the second one and the
                // surplus is taken out of the sound a pitch period at a time, below.
                self.decode(&a, DecodeKind::Normal);
                if level_db(&self.pcm) >= SKIP_SILENT_DB {
                    self.queue.extend_from_slice(&self.pcm);
                    if let Some(st) = &mut self.stereo {
                        st.queue.extend_from_slice(&st.side);
                    }
                }
                Some(self.decode(&b, DecodeKind::Normal))
            }
            Playout::Fec(p) => Some(self.decode(&p, DecodeKind::Fec)),
            Playout::Lost => Some(self.decode(&[], DecodeKind::Concealed)),
        };
        if !self.queue.is_empty() {
            self.shorten();
        }
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
        if let Some(st) = &mut self.stereo {
            for s in &mut st.side {
                *s *= self.volume;
            }
        }
        self.resampler.process(&self.pcm, out)?;
        Ok(kind)
    }

    /// Plays from the queue while it holds surplus sound: this tick's frame goes to its
    /// end, at most one [`Cut`] shortens it, and its first 20 ms are what plays now.
    fn shorten(&mut self) {
        self.queue.extend_from_slice(&self.pcm);
        let cut = Cut::find(&self.queue, self.queue.len() - FRAME_SAMPLES);
        if let Some(cut) = cut {
            cut.apply(&mut self.queue);
        }
        self.pcm.copy_from_slice(&self.queue[..FRAME_SAMPLES]);
        self.queue.drain(..FRAME_SAMPLES);
        if let Some(st) = &mut self.stereo {
            st.queue.extend_from_slice(&st.side);
            if let Some(cut) = cut {
                cut.apply(&mut st.queue);
            }
            // The two queues are cut alike; a difference would be a logic error, and
            // the side signal is then dropped rather than played out of step.
            if st.queue.len() == self.queue.len() + FRAME_SAMPLES {
                st.side.copy_from_slice(&st.queue[..FRAME_SAMPLES]);
                st.queue.drain(..FRAME_SAMPLES);
            } else {
                st.side.fill(0.0);
                st.queue.clear();
                st.queue.resize(self.queue.len(), 0.0);
            }
        }
    }

    fn decode(&mut self, packet: &[u8], kind: DecodeKind) -> DecodeKind {
        let decoder = &mut self.decoder;
        let buf: &mut [f32] = match &mut self.stereo {
            Some(st) => &mut st.wide,
            None => &mut self.pcm,
        };
        let res = match kind {
            DecodeKind::Normal => decoder.decode(packet, buf),
            DecodeKind::Fec => decoder.decode_fec(packet, buf),
            DecodeKind::Concealed => decoder.conceal(buf),
        };
        let kind = match res {
            Ok(n) if n == FRAME_SAMPLES => kind,
            Ok(_) | Err(_) => {
                // Corrupt or unexpected frame size: render concealment instead.
                self.stats.decode_errors += 1;
                if decoder.conceal(buf).is_err() {
                    buf.fill(0.0);
                }
                DecodeKind::Concealed
            }
        };
        if let Some(st) = &mut self.stereo {
            for (i, lr) in st.wide.as_chunks::<2>().0.iter().enumerate() {
                self.pcm[i] = (lr[0] + lr[1]) * 0.5;
                st.side[i] = (lr[0] - lr[1]) * 0.5;
            }
        }
        kind
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
    fn delay_correction_does_not_cut_the_waveform() {
        let mut tx = VoiceSender::new(CODEC_RATE, EncoderConfig::default()).unwrap();
        let mut rx = VoiceReceiver::new(CODEC_RATE, JitterConfig::default()).unwrap();
        let mut phase = 0.0;
        let mut out = Vec::new();
        let mut t = 0u64;
        let mut send = |rx: &mut VoiceReceiver, t: u64| {
            tx.push_samples(&chunk(1, true, &mut phase), |f| {
                rx.push_packet_at(f.seq, f.timestamp, f.payload.to_vec(), t)
            })
            .unwrap();
        };
        for _ in 0..30 {
            send(&mut rx, t);
            rx.pull_frame(&mut out).unwrap();
            t += 20;
        }
        // The network delivers 200 ms at once. The buffer first keeps that much delay
        // (the path just proved it can stall) and gives it back once the path has been
        // calm for a while: that is the correction under test.
        for _ in 0..10 {
            send(&mut rx, t);
        }
        let settled = out.len();
        for _ in 0..300 {
            send(&mut rx, t);
            rx.pull_frame(&mut out).unwrap();
            t += 20;
        }
        let s = rx.stats();
        assert!(s.jitter.dropped >= 5, "the delay was not corrected: {s:?}");
        assert_eq!(s.jitter.concealed + s.jitter.fec, 0, "{s:?}");
        // All the surplus sound was taken out again.
        assert_eq!(rx.pending_samples(), 0);
        // The tone keeps its level through the correction: whole pitch periods were
        // removed, not two frames faded into each other out of phase.
        let level = |w: &[f32]| (w.iter().map(|v| v * v).sum::<f32>() / w.len() as f32).sqrt();
        let nominal = level(&out[settled - 4 * FRAME_SAMPLES..settled]);
        let lowest = out[settled..]
            .chunks(240)
            .map(level)
            .fold(f32::MAX, f32::min);
        assert!(
            lowest > nominal * 0.9,
            "level dipped to {lowest} (nominal {nominal})"
        );
        // A 440 Hz tone at 0.3 moves ≈ 0.017 per sample; a frame cut out of it jumps by
        // up to 0.6. Blended frames stay continuous.
        let jump = out[settled - FRAME_SAMPLES..]
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(jump < 0.08, "discontinuity of {jump} in the played signal");
    }

    #[test]
    fn a_stereo_stream_keeps_its_channels_apart() {
        // Left: 440 Hz tone, right: silence — as a share's system audio would send it.
        let mut enc =
            opus::Encoder::new(CODEC_RATE, opus::Channels::Stereo, opus::Application::Audio)
                .unwrap();
        enc.set_bitrate(opus::Bitrate::Bits(96_000)).unwrap();
        let mut stereo =
            VoiceReceiver::with_channels(CODEC_RATE, JitterConfig::default(), true).unwrap();
        let mut mono = VoiceReceiver::new(CODEC_RATE, JitterConfig::default()).unwrap();
        assert!(stereo.is_stereo() && !mono.is_stereo() && mono.side().is_none());
        let mut phase = 0.0;
        let mut packet = vec![0u8; MAX_PACKET];
        let (mut left, mut right, mut mid_mono) = (0.0f32, 0.0f32, 0.0f32);
        for n in 0..60u16 {
            let tone = chunk(1, true, &mut phase);
            let frame: Vec<f32> = tone.iter().flat_map(|s| [*s, 0.0]).collect();
            let len = enc.encode_float(&frame, &mut packet).unwrap();
            let t = u64::from(n) * 20;
            stereo.push_packet(n, packet[..len].to_vec(), t);
            mono.push_packet(n, packet[..len].to_vec(), t);
            let (mut mid, mut down) = (Vec::new(), Vec::new());
            stereo.pull_frame(&mut mid).unwrap();
            mono.pull_frame(&mut down).unwrap();
            if n >= 20 {
                let side = stereo.side().unwrap();
                let l: Vec<f32> = mid.iter().zip(side).map(|(m, s)| m + s).collect();
                let r: Vec<f32> = mid.iter().zip(side).map(|(m, s)| m - s).collect();
                left += crate::testsignal::rms(&l);
                right += crate::testsignal::rms(&r);
                mid_mono += crate::testsignal::rms(&down);
            }
        }
        let (left, right, mid_mono) = (left / 40.0, right / 40.0, mid_mono / 40.0);
        // The tone is on the left only; the mono listener hears half of it.
        assert!(left > 0.15, "left {left}");
        assert!(right < left * 0.1, "right {right} vs left {left}");
        assert!(
            (mid_mono - left / 2.0).abs() < left * 0.15,
            "mono {mid_mono} left {left}"
        );
        assert_eq!(stereo.stats().decode_errors, 0);
    }

    #[test]
    fn a_cut_removes_one_period_of_a_voiced_sound_and_at_most_the_surplus() {
        // 200 Hz "voice": period 240 samples, with a harmonic so the period is unique.
        let wave = |n: usize| -> Vec<f32> {
            (0..n)
                .map(|i| {
                    let t = i as f32 / CODEC_RATE as f32;
                    0.3 * (std::f32::consts::TAU * 200.0 * t).sin()
                        + 0.1 * (std::f32::consts::TAU * 400.0 * t).sin()
                })
                .collect()
        };
        let mut q = wave(1920);
        let cut = Cut::find(&q, 960).expect("something to remove");
        assert_eq!(cut.lag % 240, 0, "a whole number of periods: {cut:?}");
        let before = q.clone();
        cut.apply(&mut q);
        assert_eq!(q.len(), 1920 - cut.lag);
        // The result is the same periodic wave, seamlessly.
        let worst = q
            .iter()
            .zip(&before)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 0.01, "the wave changed by {worst}");
        // Never more than the surplus, even if a period is longer.
        let c = Cut::find(&before, 100).expect("small surplus");
        assert!(c.lag <= 100, "{c:?}");
        // Silence: a plain short cut. Nothing to remove: none.
        let silent = vec![0.0f32; 1920];
        assert_eq!(Cut::find(&silent, 960).map(|c| c.lag), Some(Cut::PLAIN));
        assert_eq!(Cut::find(&before, 0), None);
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
