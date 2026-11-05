//! Interface scale (spec §9: interface/text scaling) on top of the system DPI. Slint's
//! winit backend takes an absolute scale factor from `SLINT_SCALE_FACTOR` when it creates
//! a window, so the user's percentage is multiplied by the system scale and put there
//! before the first window exists. A changed setting therefore applies at the next start.
//! With a user scale the factor no longer follows per-monitor DPI changes (moving the
//! window to a monitor with another scale); at 100 % nothing is overridden.

/// The system scale (1.0 = 96 DPI). Valid once the process is DPI aware, i.e. after the
/// Slint backend (winit event loop) was selected.
pub fn system_scale() -> f32 {
    imp::system_dpi() as f32 / 96.0
}

/// The factor for `percent` of the system scale, `None` at 100 % (no override).
pub fn factor(percent: u32, system: f32) -> Option<f32> {
    (percent != 100).then(|| system * percent as f32 / 100.0)
}

/// Sets `SLINT_SCALE_FACTOR` for `percent`, unless the environment already sets one
/// (tests and trials choose factors directly). Returns the factor in effect, if any.
///
/// Must run while the process has a single thread: before any session, messenger or
/// device thread is started (environment writes are not synchronised with readers).
pub fn apply(percent: u32) -> Option<f32> {
    if let Some(f) = std::env::var("SLINT_SCALE_FACTOR")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        return Some(f);
    }
    let f = factor(percent, system_scale())?;
    set_scale_env(f);
    Some(f)
}

#[allow(unsafe_code)]
fn set_scale_env(f: f32) {
    // SAFETY: called from `apply`, whose contract is that no other thread exists yet.
    unsafe { std::env::set_var("SLINT_SCALE_FACTOR", format!("{f:.3}")) };
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod imp {
    pub fn system_dpi() -> u32 {
        match unsafe { windows::Win32::UI::HiDpi::GetDpiForSystem() } {
            0 => 96,
            d => d,
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn system_dpi() -> u32 {
        96
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_multiplies_the_system_scale() {
        assert_eq!(factor(100, 1.5), None);
        assert_eq!(factor(150, 1.0), Some(1.5));
        assert_eq!(factor(200, 1.25), Some(2.5));
        assert!(system_scale() >= 1.0);
    }
}
