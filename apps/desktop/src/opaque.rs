//! Opaque application windows on Windows.
//!
//! Slint's winit backend asks for a *transparent* window on every platform but macOS, and
//! winit then enables DWM "blur behind" with an empty region: the compositor blends the
//! window with the desktop by the alpha of its surface. Our windows are opaque by design,
//! so that mode buys nothing and has a cost: whenever the surface is not fully painted
//! (after sleep, a display change, a compositor restart, in full screen) the window shows
//! through instead of keeping its last picture — and the software renderer repaints only
//! what changed. The client therefore creates its windows opaque (window attributes hook)
//! and, because Slint re-creates a hidden window without that hook, also switches the
//! mode off on the live window after every show.

/// Turns DWM blur-behind off for the window (`hwnd` from the window handle).
pub fn disable_blur_behind(hwnd: isize) -> bool {
    imp::disable_blur_behind(hwnd)
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod imp {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{DWM_BB_ENABLE, DWM_BLURBEHIND, DwmEnableBlurBehindWindow};

    pub fn disable_blur_behind(hwnd: isize) -> bool {
        let bb = DWM_BLURBEHIND {
            dwFlags: DWM_BB_ENABLE,
            fEnable: false.into(),
            ..Default::default()
        };
        // SAFETY: plain call with a window handle; a stale handle only fails the call.
        unsafe { DwmEnableBlurBehindWindow(HWND(hwnd as *mut core::ffi::c_void), &bb) }.is_ok()
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn disable_blur_behind(_hwnd: isize) -> bool {
        false
    }
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn a_window_that_does_not_exist_is_refused_without_harm() {
        assert!(!super::disable_blur_behind(0));
    }
}
