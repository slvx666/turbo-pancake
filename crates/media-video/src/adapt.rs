//! Automatic quality of our share's high layer (S4, spec §5.1: adapt to the network and
//! keep the frame rate before the resolution).
//!
//! Once a second the owner feeds what the uplink reports — loss of our main video in the
//! server's receiver reports and frames the pacer had to drop — and gets back a new
//! target (size, fps, bitrate) when it should change. The bitrate moves multiplicatively
//! (−30 % on congestion, +8 % after 3 clean seconds, never above the preset); the picture
//! follows the bitrate down a ladder of sizes at the preset frame rate (preset size →
//! 1080p → 720p → 540p) and only below the smallest size lowers the frame rate (½ steps,
//! not below 15 fps). Going down happens at once (at most every 2 s), going up a rung at
//! most every 10 s, so a size change (a new encoder and a keyframe) stays rare.

use std::time::{Duration, Instant};

/// Bits per pixel and frame a rung needs at least (screen content compresses well:
/// 1080p60 ≈ 2.1 Mbit/s, 1440p120 ≈ 7.5 Mbit/s, 720p60 ≈ 0.94 Mbit/s).
pub const MIN_BITS_PER_PIXEL: f64 = 0.017;
/// Lowest frame rate the ladder goes to.
pub const MIN_FPS: u32 = 15;

/// One configuration of the high layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rung {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Rung {
    pub fn min_kbps(&self) -> u32 {
        (f64::from(self.width) * f64::from(self.height) * f64::from(self.fps) * MIN_BITS_PER_PIXEL
            / 1000.0) as u32
    }
}

/// What the high layer should be now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub rung: Rung,
    pub bitrate_kbps: u32,
}

/// Largest even size of `src`'s aspect that fits into `max` (never larger than `src`).
pub fn fit(src: (u32, u32), max: (u32, u32)) -> (u32, u32) {
    let (sw, sh) = (src.0.max(2), src.1.max(2));
    let scale = (f64::from(max.0) / f64::from(sw))
        .min(f64::from(max.1) / f64::from(sh))
        .min(1.0);
    let even = |v: f64| ((v.round() as u32) & !1).max(2);
    (even(f64::from(sw) * scale), even(f64::from(sh) * scale))
}

/// The ladder for a preset (size, fps) and a source picture: best first.
pub fn ladder(src: (u32, u32), preset: Rung) -> Vec<Rung> {
    let mut sizes = vec![fit(src, (preset.width, preset.height))];
    for max in [(1920, 1080), (1280, 720), (960, 540)] {
        let s = fit(src, max);
        if s.0 * s.1 < sizes.last().map_or(u32::MAX, |l| l.0 * l.1) {
            sizes.push(s);
        }
    }
    let mut out: Vec<Rung> = sizes
        .iter()
        .map(|&(width, height)| Rung {
            width,
            height,
            fps: preset.fps,
        })
        .collect();
    let smallest = *out.last().expect("one size at least");
    let mut fps = preset.fps / 2;
    while fps >= MIN_FPS {
        out.push(Rung { fps, ..smallest });
        fps /= 2;
    }
    out
}

#[derive(Debug, Clone)]
pub struct Adapter {
    ladder: Vec<Rung>,
    max_kbps: u32,
    rung: usize,
    kbps: f64,
    clean_since: Option<Instant>,
    last_raise: Option<Instant>,
    last_down: Option<Instant>,
    last_up: Option<Instant>,
}

impl Adapter {
    pub const DOWN_LOSS: f32 = 0.05;
    pub const CLEAN_LOSS: f32 = 0.02;
    pub const RAISE_AFTER: Duration = Duration::from_secs(3);
    pub const DOWN_EVERY: Duration = Duration::from_secs(2);
    pub const UP_EVERY: Duration = Duration::from_secs(10);

    /// Starts at the preset (the best rung, the preset bitrate).
    pub fn new(src: (u32, u32), preset: Rung, preset_kbps: u32) -> Self {
        Self {
            ladder: ladder(src, preset),
            max_kbps: preset_kbps.max(1),
            rung: 0,
            kbps: f64::from(preset_kbps.max(1)),
            clean_since: None,
            last_raise: None,
            last_down: None,
            last_up: None,
        }
    }

    pub fn target(&self) -> Target {
        Target {
            rung: self.ladder[self.rung],
            bitrate_kbps: self.kbps as u32,
        }
    }

    pub fn ladder(&self) -> &[Rung] {
        &self.ladder
    }

    /// One report window: the loss of our video the remote reported (`None` = no report)
    /// and whether the pacer dropped frames. Returns the new target if it changed.
    pub fn update(
        &mut self,
        now: Instant,
        loss: Option<f32>,
        pacing_drops: bool,
    ) -> Option<Target> {
        let before = self.target();
        let congested = pacing_drops || loss.is_some_and(|l| l >= Self::DOWN_LOSS);
        let floor = f64::from(self.ladder.last().expect("ladder").min_kbps().max(100));
        if congested {
            self.kbps = (self.kbps * 0.7).max(floor);
            self.clean_since = None;
        } else if loss.is_none_or(|l| l <= Self::CLEAN_LOSS) {
            let since = *self.clean_since.get_or_insert(now);
            let raised_long_ago = self
                .last_raise
                .is_none_or(|t| now.saturating_duration_since(t) >= Self::RAISE_AFTER);
            if now.saturating_duration_since(since) >= Self::RAISE_AFTER && raised_long_ago {
                self.kbps = (self.kbps * 1.08).min(f64::from(self.max_kbps));
                self.last_raise = Some(now);
            }
        } else {
            // Some loss, not congestion: hold.
            self.clean_since = None;
        }
        // The best rung the bitrate pays for.
        let fits = self
            .ladder
            .iter()
            .position(|r| f64::from(r.min_kbps()) <= self.kbps)
            .unwrap_or(self.ladder.len() - 1);
        if fits > self.rung {
            if self
                .last_down
                .is_none_or(|t| now.saturating_duration_since(t) >= Self::DOWN_EVERY)
            {
                self.rung = fits;
                self.last_down = Some(now);
            }
        } else if fits < self.rung {
            let since_change = [self.last_down, self.last_up]
                .into_iter()
                .flatten()
                .map(|t| now.saturating_duration_since(t))
                .min();
            if since_change.is_none_or(|d| d >= Self::UP_EVERY) {
                // One rung at a time.
                self.rung -= 1;
                self.last_up = Some(now);
            }
        }
        let after = self.target();
        let bitrate_moved =
            before.bitrate_kbps.abs_diff(after.bitrate_kbps) * 20 > before.bitrate_kbps.max(1);
        (after.rung != before.rung || bitrate_moved).then_some(after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QHD120: Rung = Rung {
        width: 2560,
        height: 1440,
        fps: 120,
    };

    #[test]
    fn fit_keeps_aspect_and_never_upscales() {
        assert_eq!(fit((2560, 1440), (1920, 1080)), (1920, 1080));
        assert_eq!(fit((1920, 1080), (2560, 1440)), (1920, 1080));
        assert_eq!(fit((1920, 1200), (1920, 1080)), (1728, 1080));
        assert_eq!(fit((3440, 1440), (320, 320)), (320, 134));
    }

    #[test]
    fn ladder_lowers_size_before_frame_rate() {
        let l = ladder((2560, 1440), QHD120);
        let sizes: Vec<(u32, u32, u32)> = l.iter().map(|r| (r.width, r.height, r.fps)).collect();
        assert_eq!(
            sizes,
            vec![
                (2560, 1440, 120),
                (1920, 1080, 120),
                (1280, 720, 120),
                (960, 540, 120),
                (960, 540, 60),
                (960, 540, 30),
                (960, 540, 15),
            ]
        );
        // A 1080p monitor with the 1440p preset starts at its own size.
        let l = ladder((1920, 1080), QHD120);
        assert_eq!((l[0].width, l[0].height), (1920, 1080));
        assert_eq!(l[1].width, 1280);
    }

    #[test]
    fn congestion_steps_down_and_clean_network_recovers() {
        let t0 = Instant::now();
        let s = |n: u64| t0 + Duration::from_secs(n);
        let mut a = Adapter::new((2560, 1440), QHD120, 20_000);
        assert_eq!(a.target().rung, QHD120);
        // Clean at the preset: nothing to do.
        assert_eq!(a.update(s(1), Some(0.0), false), None);
        // Heavy loss for a while: bitrate falls, then the size, the frame rate last.
        let mut seen = Vec::new();
        for n in 2..20 {
            if let Some(t) = a.update(s(n), Some(0.15), false) {
                seen.push(t);
            }
        }
        let last = a.target();
        assert!(last.bitrate_kbps < 1500, "{last:?}");
        let fps_drop = seen.iter().position(|t| t.rung.fps < 120);
        let size_540 = seen.iter().position(|t| t.rung.width == 960);
        assert!(size_540.is_some() && fps_drop.is_some());
        assert!(size_540 < fps_drop, "resolution goes before fps: {seen:?}");
        // Recovery: clean reports raise the bitrate and climb one rung per 10 s.
        let mut climbed = Vec::new();
        for n in 20..400 {
            if let Some(t) = a.update(s(n), Some(0.0), false) {
                climbed.push((n, t.rung));
            }
        }
        assert_eq!(a.target().rung, QHD120, "back at the preset");
        assert_eq!(a.target().bitrate_kbps, 20_000);
        let ups: Vec<u64> = climbed
            .windows(2)
            .filter(|w| w[0].1 != w[1].1)
            .map(|w| w[1].0)
            .collect();
        assert!(
            ups.windows(2).all(|w| w[1] - w[0] >= 10),
            "one rung per 10 s at most: {ups:?}"
        );
        // Pacer drops count as congestion.
        let before = a.target().bitrate_kbps;
        a.update(s(400), None, true);
        assert!(a.target().bitrate_kbps < before);
    }
}
