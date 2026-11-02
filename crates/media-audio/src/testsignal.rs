//! Deterministic synthetic signals for tests and benchmarks (no recordings in the repo).

use std::f32::consts::TAU;

/// Small deterministic PRNG (xorshift64*), good enough for noise and loss patterns.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
}

/// Speech-like test signal: a voiced source with gliding pitch (110–220 Hz) and
/// harmonics shaped by three moving formants, a 4 Hz syllable envelope, and a 300 ms
/// pause every 2 s. Opus/SILK voice activity detection treats it as speech, unlike a
/// stationary tone. Peak ≈ 0.5.
pub fn speech_like(rate: u32, secs: f32, seed: u64) -> Vec<f32> {
    let n = (rate as f32 * secs) as usize;
    let mut rng = Rng::new(seed);
    let mut phase = 0.0f32;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f32 / rate as f32;
        let f0 = 165.0 + 55.0 * (TAU * 0.7 * t).sin();
        phase = (phase + f0 / rate as f32).fract();
        let formants = [
            700.0 + 250.0 * (TAU * 1.3 * t).sin(),
            1200.0 + 500.0 * (TAU * 0.9 * t).sin(),
            2600.0,
        ];
        let mut v = 0.0;
        let mut h = 1.0;
        while h * f0 < 4000.0 {
            let f = h * f0;
            let gain: f32 = formants
                .iter()
                .map(|&fm| 1.0 / (1.0 + ((f - fm) / 150.0).powi(2)))
                .sum::<f32>()
                / h.sqrt();
            v += gain * (TAU * h * phase).sin();
            h += 1.0;
        }
        let syllable = (0.5 - 0.5 * (TAU * 4.0 * t).cos()).powf(0.7);
        let pause = if t % 2.0 > 1.7 { 0.0 } else { 1.0 };
        let noise = (rng.unit() - 0.5) * 0.004;
        out.push(0.12 * v * syllable * pause + noise);
    }
    out
}

pub fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}
