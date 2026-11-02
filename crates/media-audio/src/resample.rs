//! Mono sample-rate conversion between device rates and [`crate::CODEC_RATE`].
//!
//! Uses rubato's synchronous FFT resampler (fixed ratio, high quality). Device clock
//! drift against the remote sender is absorbed by the jitter buffer, not here.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};

use crate::AudioError;

/// Streaming mono resampler: feed any number of samples, get every sample that can be
/// produced so far. Buffers are allocated once in [`MonoResampler::new`].
pub struct MonoResampler {
    inner: Option<Fft<f32>>,
    pending: Vec<f32>,
    out: Vec<f32>,
    rate_in: u32,
    rate_out: u32,
}

impl MonoResampler {
    pub fn new(rate_in: u32, rate_out: u32) -> Result<Self, AudioError> {
        if rate_in == 0 || rate_out == 0 {
            return Err(AudioError::Resampler(format!(
                "invalid rates {rate_in} -> {rate_out}"
            )));
        }
        if rate_in == rate_out {
            return Ok(Self {
                inner: None,
                pending: Vec::new(),
                out: Vec::new(),
                rate_in,
                rate_out,
            });
        }
        // 10 ms input chunks keep the added latency low.
        let chunk = (rate_in / 100).max(1) as usize;
        let inner = Fft::<f32>::new(
            rate_in as usize,
            rate_out as usize,
            chunk,
            1,
            FixedSync::Input,
        )
        .map_err(|e| AudioError::Resampler(e.to_string()))?;
        let out = vec![0.0; inner.output_frames_max()];
        Ok(Self {
            pending: Vec::with_capacity(inner.input_frames_max() * 4),
            out,
            inner: Some(inner),
            rate_in,
            rate_out,
        })
    }

    pub fn rate_in(&self) -> u32 {
        self.rate_in
    }

    pub fn rate_out(&self) -> u32 {
        self.rate_out
    }

    /// Algorithmic delay in output samples.
    pub fn delay(&self) -> usize {
        self.inner.as_ref().map_or(0, |r| r.output_delay())
    }

    /// Resamples `input` and appends all produced samples to `output`.
    pub fn process(&mut self, input: &[f32], output: &mut Vec<f32>) -> Result<(), AudioError> {
        let Some(inner) = self.inner.as_mut() else {
            output.extend_from_slice(input);
            return Ok(());
        };
        self.pending.extend_from_slice(input);
        let mut consumed = 0;
        loop {
            let need = inner.input_frames_next();
            if self.pending.len() - consumed < need {
                break;
            }
            let chunk = &self.pending[consumed..consumed + need];
            let buf_in = InterleavedSlice::new(chunk, 1, need)
                .map_err(|e| AudioError::Resampler(e.to_string()))?;
            let frames_out = self.out.len();
            let mut buf_out = InterleavedSlice::new_mut(&mut self.out, 1, frames_out)
                .map_err(|e| AudioError::Resampler(e.to_string()))?;
            let (used, produced) = inner
                .process_into_buffer(&buf_in, &mut buf_out, None)
                .map_err(|e| AudioError::Resampler(e.to_string()))?;
            consumed += used;
            output.extend_from_slice(&self.out[..produced]);
        }
        self.pending.drain(..consumed);
        Ok(())
    }

    /// Drops buffered input and filter state (e.g. after a device change).
    pub fn reset(&mut self) {
        self.pending.clear();
        if let Some(r) = self.inner.as_mut() {
            r.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn sine(rate: u32, freq: f32, secs: f32) -> Vec<f32> {
        let n = (rate as f32 * secs) as usize;
        (0..n)
            .map(|i| 0.5 * (TAU * freq * i as f32 / rate as f32).sin())
            .collect()
    }

    /// Goertzel power of `freq` normalised by signal power (≈ 1 for a pure tone).
    pub(crate) fn tone_ratio(x: &[f32], rate: u32, freq: f32) -> f32 {
        let w = TAU * freq / rate as f32;
        let coeff = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for &v in x {
            let s = v + coeff * s1 - s2;
            s2 = s1;
            s1 = s;
        }
        let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
        let energy: f32 = x.iter().map(|v| v * v).sum();
        2.0 * power / (x.len() as f32 * energy.max(1e-12))
    }

    #[test]
    fn converts_common_device_rates_to_48k() {
        for rate in [44_100, 48_000, 96_000, 192_000, 16_000] {
            let input = sine(rate, 1000.0, 1.0);
            let mut r = MonoResampler::new(rate, 48_000).unwrap();
            let mut out = Vec::new();
            // Feed in odd-sized pieces like a device callback would.
            for piece in input.chunks(rate as usize / 97) {
                r.process(piece, &mut out).unwrap();
            }
            let expected = 48_000.0;
            let got = out.len() as f32;
            assert!(
                (got - expected).abs() < expected * 0.02 + 1000.0,
                "{rate}: produced {got} samples"
            );
            // Skip the filter warm-up, then check the tone survived intact.
            let steady = &out[2000..out.len() - 2000];
            let ratio = tone_ratio(steady, 48_000, 1000.0);
            assert!(ratio > 0.9, "{rate}: tone ratio {ratio}");
            let rms = (steady.iter().map(|v| v * v).sum::<f32>() / steady.len() as f32).sqrt();
            assert!((rms - 0.3536).abs() < 0.03, "{rate}: rms {rms}");
        }
    }

    #[test]
    fn upsamples_48k_to_device_rate() {
        let input = sine(48_000, 440.0, 1.0);
        let mut r = MonoResampler::new(48_000, 192_000).unwrap();
        let mut out = Vec::new();
        for piece in input.chunks(960) {
            r.process(piece, &mut out).unwrap();
        }
        assert!(out.len() > 185_000, "{}", out.len());
        let ratio = tone_ratio(&out[10_000..out.len() - 10_000], 192_000, 440.0);
        assert!(ratio > 0.9, "{ratio}");
    }
}
