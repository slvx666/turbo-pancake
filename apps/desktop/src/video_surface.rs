//! The native surface a watched share is drawn on from the GPU (spec §5.1: up to
//! 1440p120 with minimal copying): a child window inside the share window. The viewer
//! thread presents decoded pictures straight into its swap chain
//! (`media_video::win::Presenter`), so they pass neither the CPU nor the UI thread.
//!
//! The child never takes the mouse or the focus: clicks fall through to the Slint window
//! under it (double click = full screen). It is only shown while pictures really arrive
//! on the GPU path; otherwise the Slint image under it shows the CPU-converted picture.

use std::cell::Cell;

/// A child window of `parent`, created hidden.
pub struct VideoSurface {
    hwnd: isize,
    parent: isize,
    rect: Cell<(i32, i32, i32, i32)>,
    shown: Cell<bool>,
}

impl VideoSurface {
    /// `parent`: the native handle of the window to draw in. `None` where there is no
    /// native surface (other systems, or the window is gone).
    pub fn new(parent: isize) -> Option<Self> {
        let hwnd = imp::create(parent)?;
        Some(Self {
            hwnd,
            parent,
            rect: Cell::new((0, 0, 0, 0)),
            shown: Cell::new(false),
        })
    }

    /// The handle to present into.
    pub fn hwnd(&self) -> isize {
        self.hwnd
    }

    /// Position and size inside the parent's client area, physical pixels.
    pub fn place(&self, x: i32, y: i32, width: i32, height: i32) {
        let rect = (x, y, width.max(1), height.max(1));
        if self.rect.replace(rect) != rect {
            imp::place(self.hwnd, rect);
        }
    }

    pub fn show(&self, on: bool) {
        if self.shown.replace(on) != on {
            imp::show(self.hwnd, on);
        }
    }
}

impl Drop for VideoSurface {
    fn drop(&mut self) {
        imp::destroy(self.hwnd, self.parent);
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod imp {
    use std::sync::OnceLock;

    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, GetParent, IsWindow, RegisterClassW,
        SW_HIDE, SW_SHOWNA, SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos, ShowWindow, WM_ERASEBKGND,
        WM_NCHITTEST, WNDCLASSW, WS_CHILD, WS_CLIPSIBLINGS, WS_EX_NOACTIVATE,
    };
    use windows::core::{PCWSTR, w};

    const CLASS: PCWSTR = w!("turbo-pancake-video");
    /// `HTTRANSPARENT`: the point belongs to the window under this one.
    const HIT_TRANSPARENT: isize = -1;

    fn handle(h: isize) -> HWND {
        HWND(h as *mut core::ffi::c_void)
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        match msg {
            WM_NCHITTEST => LRESULT(HIT_TRANSPARENT),
            // The swap chain paints every pixel: no background flash.
            WM_ERASEBKGND => LRESULT(1),
            _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
        }
    }

    pub fn create(parent: isize) -> Option<isize> {
        static CLASS_ONCE: OnceLock<()> = OnceLock::new();
        // SAFETY: window class and window creation with values owned here; the window
        // is created on the UI thread, which pumps its messages.
        unsafe {
            if !IsWindow(Some(handle(parent))).as_bool() {
                return None;
            }
            let module = GetModuleHandleW(None).ok()?;
            CLASS_ONCE.get_or_init(|| {
                let class = WNDCLASSW {
                    lpfnWndProc: Some(wndproc),
                    hInstance: module.into(),
                    lpszClassName: CLASS,
                    ..Default::default()
                };
                RegisterClassW(&class);
            });
            let hwnd = CreateWindowExW(
                WS_EX_NOACTIVATE,
                CLASS,
                PCWSTR::null(),
                WS_CHILD | WS_CLIPSIBLINGS,
                0,
                0,
                16,
                16,
                Some(handle(parent)),
                None,
                Some(module.into()),
                None,
            )
            .ok()?;
            Some(hwnd.0 as isize)
        }
    }

    pub fn place(hwnd: isize, (x, y, w, h): (i32, i32, i32, i32)) {
        // SAFETY: plain call; a stale handle only fails it.
        let _ = unsafe {
            SetWindowPos(
                handle(hwnd),
                None,
                x,
                y,
                w,
                h,
                SWP_NOACTIVATE | SWP_NOZORDER,
            )
        };
    }

    pub fn show(hwnd: isize, on: bool) {
        // SAFETY: plain call; never activates the window.
        let _ = unsafe { ShowWindow(handle(hwnd), if on { SW_SHOWNA } else { SW_HIDE }) };
    }

    pub fn destroy(hwnd: isize, parent: isize) {
        // The parent takes its children with it; only a window that is still ours
        // (the handle value may have been reused since) is destroyed here.
        // SAFETY: plain calls with handle values.
        unsafe {
            let h = handle(hwnd);
            if IsWindow(Some(h)).as_bool() && GetParent(h).ok() == Some(handle(parent)) {
                let _ = DestroyWindow(h);
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn create(_parent: isize) -> Option<isize> {
        None
    }
    pub fn place(_hwnd: isize, _rect: (i32, i32, i32, i32)) {}
    pub fn show(_hwnd: isize, _on: bool) {}
    pub fn destroy(_hwnd: isize, _parent: isize) {}
}

#[cfg(all(test, windows))]
mod tests {
    use super::VideoSurface;

    #[test]
    fn no_surface_without_a_parent_window() {
        assert!(VideoSurface::new(0).is_none());
    }
}
