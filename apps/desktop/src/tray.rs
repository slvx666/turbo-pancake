//! Notification-area (tray) icon, spec §9: the app keeps running with its window closed,
//! a click opens it again, the menu's "Выйти" really exits. Windows uses the shell's
//! `Shell_NotifyIconW` with a hidden window of our own on the UI thread, so its messages
//! arrive through the same message loop as the Slint windows; other systems (S9) report
//! the tray as unavailable and closing the window exits.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayEvent {
    /// Left click on the icon or "Открыть" in its menu.
    Open,
    /// "Выйти" in the menu.
    Exit,
}

pub use imp::{Tray, signal_running};

/// Dispatches the messages already queued for this thread (tests without an event loop).
pub fn pump_pending() {
    imp::pump_pending();
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod imp {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use std::sync::OnceLock;

    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Shell::{
        NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AllowSetForegroundWindow, AppendMenuW, CreateIcon, CreatePopupMenu, CreateWindowExW,
        DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, FindWindowW,
        GetCursorPos, GetSystemMetrics, GetWindowThreadProcessId, HICON, IMAGE_ICON,
        LR_DEFAULTCOLOR, LoadImageW, MF_SEPARATOR, MF_STRING, MSG, PM_REMOVE, PeekMessageW,
        PostMessageW, RegisterClassW, RegisterWindowMessageW, SM_CXSMICON, SM_CYSMICON,
        SetForegroundWindow, TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage,
        WINDOW_EX_STYLE, WM_APP, WM_COMMAND, WM_CONTEXTMENU, WM_LBUTTONDBLCLK, WM_LBUTTONUP,
        WM_NULL, WM_RBUTTONUP, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
    };
    use windows::core::{PCWSTR, w};

    use super::TrayEvent;

    /// Shell → us: the icon's mouse messages (lParam = the mouse message).
    const WM_TRAY: u32 = WM_APP + 1;
    const ID_OPEN: usize = 1;
    const ID_EXIT: usize = 2;
    const CLASS: PCWSTR = w!("turbo-pancake-tray");

    struct Entry {
        handler: Rc<dyn Fn(TrayEvent)>,
        /// Kept to add the icon again when Explorer restarts (`TaskbarCreated`).
        data: Option<NOTIFYICONDATAW>,
    }

    thread_local! {
        static ENTRIES: RefCell<HashMap<isize, Entry>> = RefCell::default();
    }

    fn taskbar_created() -> u32 {
        static MSG: OnceLock<u32> = OnceLock::new();
        *MSG.get_or_init(|| unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) })
    }

    fn dispatch(hwnd: HWND, event: TrayEvent) {
        // Cloned out first: the handler may create or drop a tray itself.
        let handler = ENTRIES.with(|e| {
            e.borrow()
                .get(&(hwnd.0 as isize))
                .map(|e| e.handler.clone())
        });
        if let Some(h) = handler {
            h(event);
        }
    }

    fn show_menu(hwnd: HWND) {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else {
                return;
            };
            let _ = AppendMenuW(menu, MF_STRING, ID_OPEN, w!("Открыть turbo-pancake"));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, w!("Выйти"));
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            // Documented requirement for tray menus: without the foreground the menu
            // does not close when the user clicks elsewhere. Only on the user's click.
            let _ = SetForegroundWindow(hwnd);
            let _ = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
                pt.x,
                pt.y,
                Some(0),
                hwnd,
                None,
            );
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);
        }
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if msg == WM_TRAY {
            match lp.0 as u32 {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => dispatch(hwnd, TrayEvent::Open),
                WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                _ => {}
            }
            return LRESULT(0);
        }
        if msg == WM_COMMAND {
            match wp.0 & 0xffff {
                ID_OPEN => dispatch(hwnd, TrayEvent::Open),
                ID_EXIT => dispatch(hwnd, TrayEvent::Exit),
                _ => {}
            }
            return LRESULT(0);
        }
        if msg == taskbar_created() {
            let data = ENTRIES.with(|e| e.borrow().get(&(hwnd.0 as isize)).and_then(|e| e.data));
            if let Some(d) = data {
                let _ = unsafe { Shell_NotifyIconW(NIM_ADD, &d) };
            }
        }
        unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
    }

    /// The executable's own icon (resource id 1, `crates/win-icon`) at the small-icon
    /// size of the current DPI, else a drawn fallback (test binaries have no resource).
    fn app_icon() -> Option<HICON> {
        unsafe {
            let module = GetModuleHandleW(None).ok()?;
            let loaded = LoadImageW(
                Some(module.into()),
                // MAKEINTRESOURCE(1): an integer resource id, not a real pointer.
                PCWSTR(std::ptr::without_provenance::<u16>(1)),
                IMAGE_ICON,
                GetSystemMetrics(SM_CXSMICON),
                GetSystemMetrics(SM_CYSMICON),
                LR_DEFAULTCOLOR,
            );
            match loaded {
                Ok(h) if !h.is_invalid() => Some(HICON(h.0)),
                _ => make_icon(),
            }
        }
    }

    /// A 32×32 round icon with a light ring, drawn here: the fallback when the
    /// executable carries no icon resource.
    fn make_icon() -> Option<HICON> {
        const N: usize = 32;
        let mut bgra = vec![0u8; N * N * 4];
        let c = (N as f32 - 1.0) / 2.0;
        for y in 0..N {
            for x in 0..N {
                let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
                let alpha = (15.5 - d).clamp(0.0, 1.0);
                let ring = (d - 9.0).abs() < 1.6;
                let (r, g, b) = if ring {
                    (255, 255, 255)
                } else {
                    (0x58, 0x65, 0xf2)
                };
                let i = (y * N + x) * 4;
                bgra[i..i + 4].copy_from_slice(&[b, g, r, (alpha * 255.0) as u8]);
            }
        }
        let and_mask = [0u8; N * N / 8];
        unsafe {
            let module = GetModuleHandleW(None).ok()?;
            CreateIcon(
                Some(module.into()),
                N as i32,
                N as i32,
                1,
                32,
                and_mask.as_ptr(),
                bgra.as_ptr(),
            )
            .ok()
        }
    }

    /// The hidden window's title names the profile, so another launch of the same
    /// profile finds exactly this window (and other profiles never match).
    fn window_title(instance: &str) -> Vec<u16> {
        format!("turbo-pancake tray {instance}")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect()
    }

    /// Asks the running client of `instance` to show its window (the same message as a
    /// click on its icon) and lets it take the foreground: the user started this launch,
    /// so the foreground right is ours to hand over. Returns that client's process id.
    pub fn signal_running(instance: &str) -> Option<u32> {
        let title = window_title(instance);
        unsafe {
            let hwnd = FindWindowW(CLASS, PCWSTR(title.as_ptr())).ok()?;
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            let _ = AllowSetForegroundWindow(pid);
            PostMessageW(
                Some(hwnd),
                WM_TRAY,
                WPARAM(0),
                LPARAM(WM_LBUTTONUP as isize),
            )
            .ok()?;
            Some(pid)
        }
    }

    pub struct Tray {
        hwnd: HWND,
        icon: Option<HICON>,
        shown: bool,
    }

    impl Tray {
        /// Creates the hidden message window on the calling (UI) thread and, with
        /// `show_icon`, adds the icon. Without a shell on this desktop (e.g. an isolated
        /// test desktop) the window still exists but [`Tray::icon_shown`] is false.
        pub fn new(
            tooltip: &str,
            instance: &str,
            show_icon: bool,
            handler: impl Fn(TrayEvent) + 'static,
        ) -> Result<Tray, String> {
            static CLASS_ONCE: OnceLock<()> = OnceLock::new();
            let title = window_title(instance);
            let _ = taskbar_created();
            unsafe {
                let module = GetModuleHandleW(None).map_err(|e| e.message())?;
                CLASS_ONCE.get_or_init(|| {
                    let class = WNDCLASSW {
                        lpfnWndProc: Some(wndproc),
                        hInstance: module.into(),
                        lpszClassName: CLASS,
                        ..Default::default()
                    };
                    RegisterClassW(&class);
                });
                // An ordinary (never shown) top-level window, not a message-only one:
                // only those receive the `TaskbarCreated` broadcast.
                let hwnd = CreateWindowExW(
                    WINDOW_EX_STYLE(WS_EX_TOOLWINDOW.0),
                    CLASS,
                    PCWSTR(title.as_ptr()),
                    WS_POPUP,
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    Some(module.into()),
                    None,
                )
                .map_err(|e| e.message())?;
                let mut tray = Tray {
                    hwnd,
                    icon: None,
                    shown: false,
                };
                let mut data = None;
                if show_icon {
                    tray.icon = app_icon();
                    let mut d = NOTIFYICONDATAW {
                        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                        hWnd: hwnd,
                        uID: 1,
                        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
                        uCallbackMessage: WM_TRAY,
                        hIcon: tray.icon.unwrap_or_default(),
                        ..Default::default()
                    };
                    for (dst, src) in d.szTip.iter_mut().zip(tooltip.encode_utf16().take(127)) {
                        *dst = src;
                    }
                    tray.shown = Shell_NotifyIconW(NIM_ADD, &d).as_bool();
                    data = Some(d);
                }
                ENTRIES.with(|e| {
                    e.borrow_mut().insert(
                        hwnd.0 as isize,
                        Entry {
                            handler: Rc::new(handler),
                            data,
                        },
                    )
                });
                Ok(tray)
            }
        }

        /// Whether the shell shows the icon, i.e. whether a hidden window can be found
        /// again by the user.
        pub fn icon_shown(&self) -> bool {
            self.shown
        }

        /// Queues the message a click on the icon (`Open`) or on the menu's "Выйти"
        /// (`Exit`) would send; it is handled by the message loop like the real one.
        pub fn post(&self, event: TrayEvent) {
            let (msg, wp, lp) = match event {
                TrayEvent::Open => (WM_TRAY, 0, WM_LBUTTONUP as isize),
                TrayEvent::Exit => (WM_COMMAND, ID_EXIT, 0),
            };
            let _ = unsafe { PostMessageW(Some(self.hwnd), msg, WPARAM(wp), LPARAM(lp)) };
        }
    }

    impl Drop for Tray {
        fn drop(&mut self) {
            let entry = ENTRIES.with(|e| e.borrow_mut().remove(&(self.hwnd.0 as isize)));
            unsafe {
                if self.shown
                    && let Some(d) = entry.and_then(|e| e.data)
                {
                    let _ = Shell_NotifyIconW(NIM_DELETE, &d);
                }
                let _ = DestroyWindow(self.hwnd);
                if let Some(icon) = self.icon {
                    let _ = DestroyIcon(icon);
                }
            }
        }
    }

    pub fn pump_pending() {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::TrayEvent;

    /// No tray before S9 (ADR 0012): closing the window exits.
    pub struct Tray;

    impl Tray {
        pub fn new(
            _tooltip: &str,
            _instance: &str,
            _show_icon: bool,
            _handler: impl Fn(TrayEvent) + 'static,
        ) -> Result<Tray, String> {
            Err("tray not implemented on this system".into())
        }
        pub fn icon_shown(&self) -> bool {
            false
        }
        pub fn post(&self, _event: TrayEvent) {}
    }

    pub fn signal_running(_instance: &str) -> Option<u32> {
        None
    }

    pub fn pump_pending() {}
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn clicks_and_menu_commands_reach_the_handler() {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let s = seen.clone();
        // No icon: routing only, nothing appears in the user's notification area.
        let tray = Tray::new("test", "tray-routing-test", false, move |e| {
            s.borrow_mut().push(e)
        })
        .unwrap();
        assert!(!tray.icon_shown());
        tray.post(TrayEvent::Open);
        tray.post(TrayEvent::Exit);
        pump_pending();
        assert_eq!(*seen.borrow(), [TrayEvent::Open, TrayEvent::Exit]);
        drop(tray);
        // A dropped tray's window is gone: nothing is delivered any more.
        pump_pending();
        assert_eq!(seen.borrow().len(), 2);
    }

    #[test]
    fn another_launch_finds_only_its_own_profile() {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let s = seen.clone();
        let tray = Tray::new("test", "signal-test-a", false, move |e| {
            s.borrow_mut().push(e)
        })
        .unwrap();
        assert_eq!(signal_running("signal-test-b"), None, "other profile");
        assert_eq!(signal_running("signal-test-a"), Some(std::process::id()));
        pump_pending();
        assert_eq!(*seen.borrow(), [TrayEvent::Open]);
        drop(tray);
        assert_eq!(signal_running("signal-test-a"), None, "gone with the tray");
    }

    /// Adds and removes a real icon in the shell's notification area (no window, no
    /// focus, no input). Ignored by default: it touches the user's taskbar.
    #[test]
    #[ignore]
    fn real_icon_is_added_and_removed() {
        let tray = Tray::new("turbo-pancake test", "tray-icon-test", true, |_| {}).unwrap();
        assert!(tray.icon_shown(), "Shell_NotifyIconW(NIM_ADD) failed");
        println!("TURBO_PANCAKE_TRAY_ICON shown={}", tray.icon_shown());
    }
}
