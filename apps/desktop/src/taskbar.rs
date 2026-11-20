//! The main window's taskbar button (ADR 0021): the unread badge as an overlay icon
//! (`ITaskbarList3::SetOverlayIcon`, a red circle with 1–9 or "9+", drawn here with GDI),
//! flashing for attention (`FlashWindowEx`) and whether the window is in front.
//!
//! The overlay is set on the UI thread (COM apartment of the winit event loop; we only
//! join it, tolerantly). Flash and focus are plain Win32 calls and work from any thread.

/// The text of the badge: `None` hides it.
pub fn badge_text(unread: u32) -> Option<String> {
    match unread {
        0 => None,
        1..=9 => Some(unread.to_string()),
        _ => Some("9+".into()),
    }
}

pub use imp::{flash, is_foreground, set_overlay};

#[cfg(windows)]
#[allow(unsafe_code)]
mod imp {
    use std::cell::RefCell;

    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::Graphics::Gdi::{
        ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS,
        CreateBitmap, CreateCompatibleDC, CreateDIBSection, CreateFontW, DEFAULT_CHARSET,
        DIB_RGB_COLORS, DT_CENTER, DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject, DrawTextW,
        FW_BOLD, GdiFlush, HBITMAP, HDC, OUT_DEFAULT_PRECIS, SelectObject, SetBkMode, SetTextColor,
        TRANSPARENT,
    };
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    };
    use windows::Win32::UI::Shell::{ITaskbarList3, TaskbarList};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateIconIndirect, DestroyIcon, FLASHW_TIMERNOFG, FLASHW_TRAY, FLASHWINFO, FlashWindowEx,
        GetForegroundWindow, GetSystemMetrics, HICON, ICONINFO, IsIconic, IsWindowVisible,
        SM_CXSMICON,
    };
    use windows::core::{PCWSTR, w};

    thread_local! {
        /// `None` inside: COM or the taskbar is unavailable on this thread.
        static TASKBAR: RefCell<Option<Option<ITaskbarList3>>> = const { RefCell::new(None) };
    }

    fn taskbar() -> Option<ITaskbarList3> {
        TASKBAR.with_borrow_mut(|slot| {
            slot.get_or_insert_with(|| unsafe {
                // winit already made this thread an STA (OLE): S_FALSE, or
                // RPC_E_CHANGED_MODE elsewhere — both leave COM usable.
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                let list: ITaskbarList3 =
                    CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER).ok()?;
                list.HrInit().ok()?;
                Some(list)
            })
            .clone()
        })
    }

    fn hwnd(h: isize) -> HWND {
        HWND(h as *mut _)
    }

    /// Sets (or with 0, clears) the badge on the window's taskbar button. UI thread.
    /// Fails quietly while the button does not exist yet; call again later.
    pub fn set_overlay(window: isize, unread: u32) -> bool {
        if window == 0 {
            return false;
        }
        let Some(list) = taskbar() else {
            return false;
        };
        let text = super::badge_text(unread);
        let icon = text.as_deref().and_then(draw_badge);
        let label: Vec<u16> = format!("{unread} непрочитанных")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let ok = unsafe {
            list.SetOverlayIcon(
                hwnd(window),
                icon.unwrap_or_default(),
                if icon.is_some() {
                    PCWSTR(label.as_ptr())
                } else {
                    PCWSTR::null()
                },
            )
            .is_ok()
        };
        // The taskbar keeps its own copy.
        if let Some(i) = icon {
            let _ = unsafe { DestroyIcon(i) };
        }
        ok
    }

    /// Flashes the taskbar button until the window comes to the front (nothing while it
    /// already is, or while it has no button, i.e. hidden to the tray).
    pub fn flash(window: isize) {
        if window == 0 {
            return;
        }
        let info = FLASHWINFO {
            cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
            hwnd: hwnd(window),
            dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
            uCount: 0,
            dwTimeout: 0,
        };
        let _ = unsafe { FlashWindowEx(&info) };
    }

    /// The window is shown, not minimized and the one the user works in.
    pub fn is_foreground(window: isize) -> bool {
        if window == 0 {
            return false;
        }
        let h = hwnd(window);
        unsafe {
            GetForegroundWindow() == h && IsWindowVisible(h).as_bool() && !IsIconic(h).as_bool()
        }
    }

    /// A 32-bit top-down DIB section of `size`² pixels and its pixels.
    fn dib(dc: HDC, size: i32) -> Option<(HBITMAP, *mut u32)> {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bmp =
            unsafe { CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) }.ok()?;
        (!bits.is_null()).then_some((bmp, bits.cast()))
    }

    /// Red circle with white text at the small-icon size of the system DPI.
    fn draw_badge(text: &str) -> Option<HICON> {
        let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.clamp(16, 64);
        let n = size as usize;
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return None;
            }
            let result = (|| {
                // 1. The text, white on black: its brightness is its coverage.
                let (mask_bmp, mask_bits) = dib(dc, size)?;
                let old = SelectObject(dc, mask_bmp.into());
                let height = if text.len() > 1 {
                    size * 9 / 16
                } else {
                    size * 11 / 16
                };
                let font = CreateFontW(
                    -height,
                    0,
                    0,
                    0,
                    FW_BOLD.0 as i32,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET,
                    OUT_DEFAULT_PRECIS,
                    CLIP_DEFAULT_PRECIS,
                    ANTIALIASED_QUALITY,
                    0,
                    w!("Segoe UI"),
                );
                let old_font = SelectObject(dc, font.into());
                SetBkMode(dc, TRANSPARENT);
                SetTextColor(dc, windows::Win32::Foundation::COLORREF(0x00ff_ffff));
                let mut rect = RECT {
                    left: 0,
                    top: 0,
                    right: size,
                    bottom: size,
                };
                let mut wide: Vec<u16> = text.encode_utf16().collect();
                DrawTextW(
                    dc,
                    &mut wide,
                    &mut rect,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                );
                let _ = GdiFlush();
                let coverage: Vec<f32> = std::slice::from_raw_parts(mask_bits, n * n)
                    .iter()
                    .map(|p| ((p >> 8) & 0xff) as f32 / 255.0)
                    .collect();
                SelectObject(dc, old_font);
                let _ = DeleteObject(font.into());
                SelectObject(dc, old);
                let _ = DeleteObject(mask_bmp.into());

                // 2. The badge: anti-aliased red disc, text blended in white.
                let (color_bmp, color_bits) = dib(dc, size)?;
                let pixels = std::slice::from_raw_parts_mut(color_bits, n * n);
                let c = (n as f32 - 1.0) / 2.0;
                let radius = n as f32 / 2.0;
                let (r0, g0, b0) = (0xe0 as f32, 0x2f as f32, 0x3a as f32);
                for y in 0..n {
                    for x in 0..n {
                        let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
                        let alpha = (radius - d).clamp(0.0, 1.0);
                        let t = coverage[y * n + x];
                        let mix = |base: f32| (base + (255.0 - base) * t).round() as u32;
                        let a = (alpha * 255.0).round() as u32;
                        pixels[y * n + x] = (a << 24) | (mix(r0) << 16) | (mix(g0) << 8) | mix(b0);
                    }
                }
                let and_mask = vec![0u8; n.div_ceil(16) * 2 * n];
                let mask = CreateBitmap(size, size, 1, 1, Some(and_mask.as_ptr().cast()));
                let info = ICONINFO {
                    fIcon: true.into(),
                    xHotspot: 0,
                    yHotspot: 0,
                    hbmMask: mask,
                    hbmColor: color_bmp,
                };
                let icon = CreateIconIndirect(&info).ok();
                let _ = DeleteObject(mask.into());
                let _ = DeleteObject(color_bmp.into());
                icon
            })();
            let _ = DeleteDC(dc);
            result
        }
    }

    #[cfg(test)]
    mod tests {
        /// Drawing only (no taskbar call): a real icon handle comes back.
        #[test]
        fn badge_icons_are_drawn() {
            for t in ["1", "7", "9+"] {
                let icon = super::draw_badge(t).expect(t);
                let _ = unsafe { super::DestroyIcon(icon) };
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn set_overlay(_window: isize, _unread: u32) -> bool {
        false
    }
    pub fn flash(_window: isize) {}
    pub fn is_foreground(_window: isize) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_text_is_short() {
        assert_eq!(badge_text(0), None);
        assert_eq!(badge_text(1).as_deref(), Some("1"));
        assert_eq!(badge_text(9).as_deref(), Some("9"));
        assert_eq!(badge_text(10).as_deref(), Some("9+"));
        assert_eq!(badge_text(4000).as_deref(), Some("9+"));
    }

    #[test]
    fn no_window_means_no_os_call() {
        assert!(!set_overlay(0, 3));
        assert!(!is_foreground(0));
        flash(0);
    }
}
