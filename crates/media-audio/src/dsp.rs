//! Voice processing on the microphone path: WebRTC AudioProcessing (AEC3 echo
//! cancellation, noise suppression, optional adaptive gain), spec §4.
//!
//! Runs at [`crate::CODEC_RATE`] on 10 ms mono frames between the capture resampler and
//! the Opus encoder. The echo canceller needs the far-end reference: every 48 kHz mix sent
//! to the speaker is passed to [`VoiceDsp::analyze_render`]. System audio of a screen share
//! must not go through this path (spec §4 SHOULD).
//!
//! Without the `apm` cargo feature the processor is a pass-through and
//! [`DSP_AVAILABLE`] is `false`, so the UI can show that echo/noise processing is missing.

use crate::AudioError;

/// Whether this build contains WebRTC APM (`apm` feature).
pub const DSP_AVAILABLE: bool = cfg!(feature = "apm");

/// Samples in one 10 ms APM frame at 48 kHz.
pub const DSP_FRAME: usize = (crate::CODEC_RATE / 100) as usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DspConfig {
    /// On by default; the user may turn it off (spec §4).
    pub noise_suppression: bool,
    pub echo_cancellation: bool,
    /// Adaptive digital gain (AGC2). Off by default: it changes what the peers hear.
    pub auto_gain: bool,
}

impl Default for DspConfig {
    fn default() -> Self {
        Self {
            noise_suppression: true,
            echo_cancellation: true,
            auto_gain: false,
        }
    }
}

impl DspConfig {
    pub fn any(&self) -> bool {
        self.noise_suppression || self.echo_cancellation || self.auto_gain
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DspStats {
    /// Echo return loss enhancement reported by AEC3, dB.
    pub erle_db: Option<f64>,
    /// Estimated render → capture delay, ms.
    pub delay_ms: Option<u32>,
    pub capture_frames: u64,
    pub render_frames: u64,
}

pub struct VoiceDsp {
    config: DspConfig,
    stats: DspStats,
    #[cfg(feature = "apm")]
    apm: webrtc_audio_processing::Processor,
    #[cfg(feature = "apm")]
    scratch: Vec<f32>,
}

impl VoiceDsp {
    pub fn new(config: DspConfig) -> Result<Self, AudioError> {
        #[cfg(feature = "apm")]
        {
            let apm = webrtc_audio_processing::Processor::new(crate::CODEC_RATE)
                .map_err(|e| AudioError::Dsp(e.to_string()))?;
            apm.set_config(apm_config(config));
            Ok(Self {
                config,
                stats: DspStats::default(),
                apm,
                scratch: vec![0.0; DSP_FRAME],
            })
        }
        #[cfg(not(feature = "apm"))]
        Ok(Self {
            config,
            stats: DspStats::default(),
        })
    }

    pub fn config(&self) -> DspConfig {
        self.config
    }

    /// Applies a new configuration on the fly (only changed submodules are reset).
    pub fn set_config(&mut self, config: DspConfig) {
        self.config = config;
        #[cfg(feature = "apm")]
        self.apm.set_config(apm_config(config));
    }

    /// `true` when the build has APM and at least one stage is enabled.
    pub fn active(&self) -> bool {
        DSP_AVAILABLE && self.config.any()
    }

    pub fn stats(&self) -> DspStats {
        self.stats
    }

    /// Hint for AEC/AGC adaptation while the microphone is muted.
    pub fn set_muted(&mut self, _muted: bool) {
        #[cfg(feature = "apm")]
        self.apm.set_output_will_be_muted(_muted);
    }

    /// Processes microphone audio in place; `frame.len()` must be a multiple of
    /// [`DSP_FRAME`] (a 20 ms codec frame is two APM frames).
    pub fn process_capture(&mut self, frame: &mut [f32]) -> Result<(), AudioError> {
        if !self.active() {
            return Ok(());
        }
        #[cfg(feature = "apm")]
        {
            for chunk in frame.as_chunks_mut::<DSP_FRAME>().0 {
                self.apm
                    .process_capture_frame([&mut chunk[..]])
                    .map_err(|e| AudioError::Dsp(e.to_string()))?;
                self.stats.capture_frames += 1;
            }
            let st = self.apm.get_stats();
            self.stats.erle_db = st.echo_return_loss_enhancement;
            self.stats.delay_ms = st.delay_ms;
        }
        #[cfg(not(feature = "apm"))]
        let _ = frame;
        Ok(())
    }

    /// Feeds the signal sent to the speaker (48 kHz, multiple of [`DSP_FRAME`]) as the
    /// echo reference. Ignored while echo cancellation is off.
    pub fn analyze_render(&mut self, frame: &[f32]) -> Result<(), AudioError> {
        if !DSP_AVAILABLE || !self.config.echo_cancellation {
            return Ok(());
        }
        #[cfg(feature = "apm")]
        for chunk in frame.as_chunks::<DSP_FRAME>().0 {
            self.scratch.copy_from_slice(chunk);
            self.apm
                .analyze_render_frame([&self.scratch[..]])
                .map_err(|e| AudioError::Dsp(e.to_string()))?;
            self.stats.render_frames += 1;
        }
        #[cfg(not(feature = "apm"))]
        let _ = frame;
        Ok(())
    }
}

#[cfg(feature = "apm")]
fn apm_config(c: DspConfig) -> webrtc_audio_processing_config::Config {
    use webrtc_audio_processing_config as cfg;
    cfg::Config {
        echo_canceller: c.echo_cancellation.then(cfg::EchoCanceller::default),
        noise_suppression: c.noise_suppression.then(cfg::NoiseSuppression::default),
        // Removes DC and rumble; AEC3 expects it.
        high_pass_filter: c.any().then(cfg::HighPassFilter::default),
        gain_controller: c.auto_gain.then(|| {
            cfg::GainController::GainController2(cfg::GainController2 {
                adaptive_digital: Some(cfg::AdaptiveDigital::default()),
                ..cfg::GainController2::default()
            })
        }),
        ..cfg::Config::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsignal::Rng;
    #[cfg(feature = "apm")]
    use crate::testsignal::rms;

    #[test]
    fn disabled_or_unavailable_is_pass_through() {
        let off = DspConfig {
            noise_suppression: false,
            echo_cancellation: false,
            auto_gain: false,
        };
        let mut dsp = VoiceDsp::new(off).unwrap();
        assert!(!dsp.active());
        let mut rng = Rng::new(3);
        let input: Vec<f32> = (0..DSP_FRAME * 10).map(|_| rng.unit() - 0.5).collect();
        let mut x = input.clone();
        dsp.process_capture(&mut x).unwrap();
        dsp.analyze_render(&input).unwrap();
        assert_eq!(x, input);
        assert_eq!(dsp.stats().capture_frames, 0);
        assert_eq!(
            VoiceDsp::new(DspConfig::default()).unwrap().active(),
            DSP_AVAILABLE
        );
    }

    /// AEC3: far-end speech played through a 30 ms, −6 dB echo path with a silent
    /// near end must be removed from the microphone signal after convergence.
    #[cfg(feature = "apm")]
    #[test]
    fn echo_is_cancelled() {
        use crate::testsignal::speech_like;
        let far = speech_like(48_000, 8.0, 1);
        let delay = 48 * 30;
        let mut dsp = VoiceDsp::new(DspConfig::default()).unwrap();
        let (mut mic_in, mut mic_out) = (Vec::new(), Vec::new());
        for (i, chunk) in far.as_chunks::<{ DSP_FRAME * 2 }>().0.iter().enumerate() {
            dsp.analyze_render(chunk).unwrap();
            let start = i * DSP_FRAME * 2;
            let mut mic: Vec<f32> = (start..start + DSP_FRAME * 2)
                .map(|t| {
                    if t >= delay {
                        far[t - delay] * 0.5
                    } else {
                        0.0
                    }
                })
                .collect();
            mic_in.extend_from_slice(&mic);
            dsp.process_capture(&mut mic).unwrap();
            mic_out.extend_from_slice(&mic);
        }
        let tail = mic_in.len() / 2;
        let reduction = 20.0 * (rms(&mic_out[tail..]) / rms(&mic_in[tail..])).log10();
        assert!(reduction < -25.0, "echo reduced by only {reduction:.1} dB");
        assert!(dsp.stats().render_frames > 0 && dsp.stats().capture_frames > 0);
    }

    /// NS: stationary noise in speech pauses is attenuated, speech level is kept.
    #[cfg(feature = "apm")]
    #[test]
    fn noise_is_suppressed_in_pauses() {
        use crate::testsignal::speech_like;
        let speech = speech_like(48_000, 8.0, 2);
        let mut rng = Rng::new(11);
        let noisy: Vec<f32> = speech
            .iter()
            .map(|s| s + (rng.unit() - 0.5) * 0.04)
            .collect();
        let mut out = noisy.clone();
        let mut dsp = VoiceDsp::new(DspConfig {
            echo_cancellation: false,
            ..DspConfig::default()
        })
        .unwrap();
        for chunk in out.as_chunks_mut::<{ DSP_FRAME * 2 }>().0 {
            dsp.process_capture(chunk).unwrap();
        }
        // speech_like pauses: t mod 2 s in (1.7, 2.0); skip the first 4 s (adaptation).
        let pick = |x: &[f32], pause: bool| -> Vec<f32> {
            (48_000 * 4..x.len())
                .filter(|&i| {
                    let t = (i as f32 / 48_000.0) % 2.0;
                    if pause { t > 1.75 } else { t < 1.6 }
                })
                .map(|i| x[i])
                .collect()
        };
        let floor = 20.0 * (rms(&pick(&out, true)) / rms(&pick(&noisy, true))).log10();
        let speech_db = 20.0 * (rms(&pick(&out, false)) / rms(&pick(&noisy, false))).log10();
        assert!(floor < -6.0, "noise floor change {floor:.1} dB");
        assert!(speech_db > -3.0, "speech level change {speech_db:.1} dB");
    }
}
