//! Opus 48 kHz mono, 20 ms frames (libopus via the `opus` crate, ADR 0006).

use opus::{Application, Bitrate, Channels, Decoder, Encoder, Signal};

use crate::{AudioError, CODEC_RATE, FRAME_SAMPLES};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    /// Target bitrate in bits per second.
    pub bitrate: i32,
    /// In-band forward error correction (LBRR); the receiver recovers a lost frame
    /// from the next packet.
    pub fec: bool,
    /// Discontinuous transmission: near-empty packets during silence.
    pub dtx: bool,
    /// Expected packet loss in percent; drives how much FEC the encoder spends.
    pub expected_loss_percent: i32,
    /// 0..=10, encoder CPU/quality trade-off.
    pub complexity: i32,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            bitrate: 32_000,
            fec: true,
            dtx: false,
            expected_loss_percent: 5,
            complexity: 9,
        }
    }
}

pub struct VoiceEncoder {
    inner: Encoder,
    config: EncoderConfig,
}

impl VoiceEncoder {
    pub fn new(config: EncoderConfig) -> Result<Self, AudioError> {
        let mut enc = Self {
            inner: Encoder::new(CODEC_RATE, Channels::Mono, Application::Voip)?,
            config,
        };
        // Voice hint keeps the encoder in SILK/hybrid mode, where in-band FEC exists.
        enc.inner.set_signal(Signal::Voice)?;
        enc.apply(config)?;
        Ok(enc)
    }

    /// Changes bitrate/FEC/DTX on the fly (network adaptation).
    pub fn apply(&mut self, config: EncoderConfig) -> Result<(), AudioError> {
        self.inner.set_bitrate(Bitrate::Bits(config.bitrate))?;
        self.inner.set_inband_fec(config.fec)?;
        self.inner.set_dtx(config.dtx)?;
        self.inner
            .set_packet_loss_perc(config.expected_loss_percent.clamp(0, 100))?;
        self.inner.set_complexity(config.complexity.clamp(0, 10))?;
        self.config = config;
        Ok(())
    }

    pub fn config(&self) -> EncoderConfig {
        self.config
    }

    /// Encodes exactly one 20 ms frame; returns the packet length written to `out`.
    pub fn encode(&mut self, frame: &[f32], out: &mut [u8]) -> Result<usize, AudioError> {
        debug_assert_eq!(frame.len(), FRAME_SAMPLES);
        Ok(self.inner.encode_float(frame, out)?)
    }
}

/// How a 20 ms output frame was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeKind {
    Normal,
    /// Recovered from the FEC data carried by the following packet.
    Fec,
    /// Packet loss concealment (no data for this frame).
    Concealed,
}

pub struct VoiceDecoder {
    inner: Decoder,
}

impl VoiceDecoder {
    pub fn new() -> Result<Self, AudioError> {
        Ok(Self {
            inner: Decoder::new(CODEC_RATE, Channels::Mono)?,
        })
    }

    /// Decodes one packet into `out` (must hold [`FRAME_SAMPLES`]).
    pub fn decode(&mut self, packet: &[u8], out: &mut [f32]) -> Result<usize, AudioError> {
        Ok(self.inner.decode_float(packet, out, false)?)
    }

    /// Reconstructs the frame *preceding* `next_packet` from its FEC data.
    pub fn decode_fec(&mut self, next_packet: &[u8], out: &mut [f32]) -> Result<usize, AudioError> {
        Ok(self
            .inner
            .decode_float(next_packet, &mut out[..FRAME_SAMPLES], true)?)
    }

    /// Generates one concealment frame.
    pub fn conceal(&mut self, out: &mut [f32]) -> Result<usize, AudioError> {
        Ok(self
            .inner
            .decode_float(&[], &mut out[..FRAME_SAMPLES], false)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MAX_PACKET;
    use crate::testsignal::{rms, speech_like};

    fn speech_frames(n: usize) -> Vec<Vec<f32>> {
        speech_like(CODEC_RATE, n as f32 * 0.02, 7)
            .as_chunks::<FRAME_SAMPLES>()
            .0
            .iter()
            .map(|f| f.to_vec())
            .collect()
    }

    #[test]
    fn round_trip_keeps_level_and_bitrate() {
        let mut enc = VoiceEncoder::new(EncoderConfig::default()).unwrap();
        let mut dec = VoiceDecoder::new().unwrap();
        let mut pkt = [0u8; MAX_PACKET];
        let mut out = vec![0.0; FRAME_SAMPLES];
        let mut bytes = 0;
        let frames = speech_frames(100);
        let mut level = 0.0;
        for (i, f) in frames.iter().enumerate() {
            let n = enc.encode(f, &mut pkt).unwrap();
            bytes += n;
            assert_eq!(dec.decode(&pkt[..n], &mut out).unwrap(), FRAME_SAMPLES);
            if i >= 10 {
                level += rms(&out);
            }
        }
        let level = level / 90.0;
        let expected = frames[10..].iter().map(|f| rms(f)).sum::<f32>() / 90.0;
        assert!(
            (level - expected).abs() < expected * 0.2,
            "{level} vs {expected}"
        );
        let kbps = bytes as f32 * 8.0 / 2.0 / 1000.0;
        assert!(kbps > 16.0 && kbps < 48.0, "{kbps} kbit/s");
    }

    #[test]
    fn fec_recovers_lost_frame_better_than_plc() {
        let mut enc = VoiceEncoder::new(EncoderConfig {
            expected_loss_percent: 20,
            ..EncoderConfig::default()
        })
        .unwrap();
        let frames = speech_frames(100);
        let mut pkts = Vec::new();
        let mut buf = [0u8; MAX_PACKET];
        for f in &frames {
            let n = enc.encode(f, &mut buf).unwrap();
            pkts.push(buf[..n].to_vec());
        }
        // Reference: decoded without loss.
        let mut reference = VoiceDecoder::new().unwrap();
        let mut ref_out = vec![0.0; FRAME_SAMPLES];
        let mut ref_frames = Vec::new();
        for p in &pkts {
            reference.decode(p, &mut ref_out).unwrap();
            ref_frames.push(ref_out.clone());
        }
        let lost = [12, 23, 31, 44, 58, 66, 79];
        let run = |use_fec: bool| -> f32 {
            let mut dec = VoiceDecoder::new().unwrap();
            let mut out = vec![0.0; FRAME_SAMPLES];
            let mut err = 0.0;
            for (i, p) in pkts.iter().enumerate() {
                if lost.contains(&i) {
                    if use_fec {
                        dec.decode_fec(&pkts[i + 1], &mut out).unwrap();
                    } else {
                        dec.conceal(&mut out).unwrap();
                    }
                    let diff: Vec<f32> =
                        out.iter().zip(&ref_frames[i]).map(|(a, b)| a - b).collect();
                    err += rms(&diff);
                } else {
                    dec.decode(p, &mut out).unwrap();
                }
            }
            err
        };
        let fec_err = run(true);
        let plc_err = run(false);
        assert!(fec_err < plc_err * 0.7, "fec {fec_err} plc {plc_err}");
    }

    #[test]
    fn dtx_shrinks_silence() {
        let mut enc = VoiceEncoder::new(EncoderConfig {
            dtx: true,
            ..EncoderConfig::default()
        })
        .unwrap();
        let silence = vec![0.0; FRAME_SAMPLES];
        let mut buf = [0u8; MAX_PACKET];
        let total: usize = (0..50)
            .map(|_| enc.encode(&silence, &mut buf).unwrap())
            .sum();
        assert!(total < 50 * 10, "{total} bytes for 1 s of silence");
    }
}
