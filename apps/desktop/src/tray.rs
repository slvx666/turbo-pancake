//! Notification-area (tray) icon, spec §9: the app keeps running with its window closed,
//! a click opens it again, the menu's "Выйти" really exits. Windows uses the shell's
//! `Shell_NotifyIconW` with a hidden window of our own on the UI thread, so its messages
//! arrive through the same message loop as the Slint windows; other systems (S9) report
//! the tray as unavailable and closing the window exits.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayEvent {
    /// The user clicked the last notification shown through [`TrayNotifier::balloon`];
    /// carries the tag given there (what was notified).
    NotificationClicked(u64),
    /// Left click on the icon or "Открыть" in its menu.
    Open,
    /// "Выйти" in the menu.
    Exit,
    /// The display configuration changed, the machine woke up or desktop composition
    /// restarted: window surfaces may have been discarded, so windows repaint in full.
    Refresh,
}

pub use imp::{Tray, TrayNotifier, signal_running};

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
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, OnceLock};

    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Shell::{
        NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_LARGE_ICON, NIIF_NOSOUND,
        NIIF_RESPECT_QUIET_TIME, NIIF_USER, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
        Shell_NotifyIconW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AllowSetForegroundWindow, AppendMenuW, CreateIcon, CreatePopupMenu, CreateWindowExW,
        DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, FindWindowW,
        GetCursorPos, GetSystemMetrics, GetWindowThreadProcessId, HICON, IMAGE_ICON,
        LR_DEFAULTCOLOR, LoadImageW, MF_SEPARATOR, MF_STRING, MSG, PM_REMOVE, PeekMessageW,
        PostMessageW, RegisterClassW, RegisterWindowMessageW, SM_CXICON, SM_CXSMICON, SM_CYICON,
        SM_CYSMICON, SYSTEM_METRICS_INDEX, SetForegroundWindow, TPM_BOTTOMALIGN, TPM_RIGHTBUTTON,
        TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE, WM_APP, WM_COMMAND, WM_CONTEXTMENU,
        WM_DISPLAYCHANGE, WM_DWMCOMPOSITIONCHANGED, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL,
        WM_POWERBROADCAST, WM_RBUTTONUP, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
    };
    use windows::core::{PCWSTR, w};

    use super::TrayEvent;

    /// Shell → us: the icon's mouse messages (lParam = the mouse message).
    const WM_TRAY: u32 = WM_APP + 1;
    const ID_OPEN: usize = 1;
    const ID_EXIT: usize = 2;
    /// `WM_POWERBROADCAST` wParam: resumed from sleep (by the user / automatically).
    const PBT_APMRESUMESUSPEND: usize = 0x0007;
    const PBT_APMRESUMEAUTOMATIC: usize = 0x0012;
    const CLASS: PCWSTR = w!("turbo-pancake-tray");
    /// Shell → us (lParam of `WM_TRAY`): the user clicked the balloon / toast.
    const NIN_BALLOONUSERCLICK: u32 = 0x0400 + 5;

    /// Tag of the newest balloon (a click reports it).
    static LAST_TAG: AtomicU64 = AtomicU64::new(0);

    /// Current tooltip per tray window, set from any thread; used when Explorer restarts.
    fn tips() -> &'static Mutex<HashMap<isize, Vec<u16>>> {
        static TIPS: OnceLock<Mutex<HashMap<isize, Vec<u16>>>> = OnceLock::new();
        TIPS.get_or_init(Mutex::default)
    }

    /// Copies `text` into a fixed, zero-terminated UTF-16 field (truncated).
    fn fill(dst: &mut [u16], text: &str) {
        dst.fill(0);
        let max = dst.len() - 1;
        for (d, s) in dst.iter_mut().zip(text.encode_utf16().take(max)) {
            *d = s;
        }
    }

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
                NIN_BALLOONUSERCLICK => dispatch(
                    hwnd,
                    TrayEvent::NotificationClicked(LAST_TAG.load(Ordering::Relaxed)),
                ),
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
        // Broadcasts to top-level windows after which the compositor may have dropped the
        // windows' surfaces; the app windows then repaint in full.
        let resumed = msg == WM_POWERBROADCAST
            && matches!(wp.0, PBT_APMRESUMESUSPEND | PBT_APMRESUMEAUTOMATIC);
        if msg == WM_DISPLAYCHANGE || msg == WM_DWMCOMPOSITIONCHANGED || resumed {
            dispatch(hwnd, TrayEvent::Refresh);
        }
        if msg == taskbar_created() {
            dispatch(hwnd, TrayEvent::Refresh);
            let data = ENTRIES.with(|e| e.borrow().get(&(hwnd.0 as isize)).and_then(|e| e.data));
            if let Some(mut d) = data {
                if let Some(tip) = tips()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&(hwnd.0 as isize))
                {
                    let n = tip.len().min(d.szTip.len() - 1);
                    d.szTip.fill(0);
                    d.szTip[..n].copy_from_slice(&tip[..n]);
                }
                let _ = unsafe { Shell_NotifyIconW(NIM_ADD, &d) };
            }
        }
        unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
    }

    /// The executable's own icon (resource id 1, `crates/win-icon`) at the small-icon
    /// size of the current DPI, else a drawn fallback (test binaries have no resource).
    fn app_icon() -> Option<HICON> {
        load_icon(SM_CXSMICON, SM_CYSMICON).or_else(make_icon)
    }

    /// The executable's icon at this size (no fallback).
    fn load_icon(cx: SYSTEM_METRICS_INDEX, cy: SYSTEM_METRICS_INDEX) -> Option<HICON> {
        unsafe {
            let module = GetModuleHandleW(None).ok()?;
            let loaded = LoadImageW(
                Some(module.into()),
                // MAKEINTRESOURCE(1): an integer resource id, not a real pointer.
                PCWSTR(std::ptr::without_provenance::<u16>(1)),
                IMAGE_ICON,
                GetSystemMetrics(cx),
                GetSystemMetrics(cy),
                LR_DEFAULTCOLOR,
            );
            match loaded {
                Ok(h) if !h.is_invalid() => Some(HICON(h.0)),
                _ => None,
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
        /// The app icon at the large size, for notifications.
        large_icon: Option<HICON>,
        shown: bool,
    }

    /// Shows notifications and changes the tooltip of a tray icon from any thread
    /// (`Shell_NotifyIconW` only messages Explorer). Valid while its [`Tray`] lives.
    #[derive(Clone, Copy, Debug)]
    pub struct TrayNotifier {
        hwnd: isize,
        large_icon: isize,
    }

    impl TrayNotifier {
        fn data(&self) -> NOTIFYICONDATAW {
            NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: HWND(self.hwnd as *mut _),
                uID: 1,
                ..Default::default()
            }
        }

        /// A notification at the icon (a toast on Windows 10/11) with the app's large
        /// icon, without the system sound (the app plays its own) and held back during
        /// quiet hours / Focus Assist. A click reports [`TrayEvent::NotificationClicked`]
        /// with `tag`.
        pub fn balloon(&self, title: &str, text: &str, tag: u64) {
            LAST_TAG.store(tag, Ordering::Relaxed);
            let mut d = self.data();
            d.uFlags = NIF_INFO;
            d.dwInfoFlags = NIIF_NOSOUND | NIIF_RESPECT_QUIET_TIME;
            if self.large_icon != 0 {
                d.dwInfoFlags |= NIIF_USER | NIIF_LARGE_ICON;
                d.hBalloonIcon = HICON(self.large_icon as *mut _);
            } else {
                d.dwInfoFlags |= NIIF_INFO;
            }
            fill(&mut d.szInfoTitle, title);
            fill(&mut d.szInfo, text);
            let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &d) };
        }

        pub fn set_tooltip(&self, text: &str) {
            let mut d = self.data();
            d.uFlags = NIF_TIP;
            fill(&mut d.szTip, text);
            tips()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(self.hwnd, text.encode_utf16().collect());
            let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &d) };
        }
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
                    large_icon: None,
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
                    if tray.shown {
                        tray.large_icon = load_icon(SM_CXICON, SM_CYICON);
                    }
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

        /// Shows a notification balloon at the icon (no-op without an icon), e.g. the
        /// one-time hint that closing the window keeps the app running here.
        pub fn notify(&self, title: &str, text: &str) {
            if !self.shown {
                return;
            }
            let mut d = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: self.hwnd,
                uID: 1,
                uFlags: NIF_INFO,
                dwInfoFlags: NIIF_INFO,
                ..Default::default()
            };
            for (dst, src) in d.szInfoTitle.iter_mut().zip(title.encode_utf16().take(63)) {
                *dst = src;
            }
            for (dst, src) in d.szInfo.iter_mut().zip(text.encode_utf16().take(255)) {
                *dst = src;
            }
            let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &d) };
        }

        /// Sends notifications / sets the tooltip from any thread; `None` without an icon.
        pub fn notifier(&self) -> Option<TrayNotifier> {
            self.shown.then(|| TrayNotifier {
                hwnd: self.hwnd.0 as isize,
                large_icon: self.large_icon.map_or(0, |i| i.0 as isize),
            })
        }

        /// Queues the message a click on the icon (`Open`) or on the menu's "Выйти"
        /// (`Exit`) would send; it is handled by the message loop like the real one.
        pub fn post(&self, event: TrayEvent) {
            let (msg, wp, lp) = match event {
                TrayEvent::Open => (WM_TRAY, 0, WM_LBUTTONUP as isize),
                TrayEvent::Exit => (WM_COMMAND, ID_EXIT, 0),
                TrayEvent::Refresh => (WM_DISPLAYCHANGE, 0, 0),
                TrayEvent::NotificationClicked(tag) => {
                    LAST_TAG.store(tag, Ordering::Relaxed);
                    (WM_TRAY, 0, NIN_BALLOONUSERCLICK as isize)
                }
            };
            let _ = unsafe { PostMessageW(Some(self.hwnd), msg, WPARAM(wp), LPARAM(lp)) };
        }
    }

    impl Drop for Tray {
        fn drop(&mut self) {
            let entry = ENTRIES.with(|e| e.borrow_mut().remove(&(self.hwnd.0 as isize)));
            tips()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&(self.hwnd.0 as isize));
            unsafe {
                if self.shown
                    && let Some(d) = entry.and_then(|e| e.data)
                {
                    let _ = Shell_NotifyIconW(NIM_DELETE, &d);
                }
                let _ = DestroyWindow(self.hwnd);
                for icon in [self.icon, self.large_icon].into_iter().flatten() {
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

    #[derive(Clone, Copy, Debug)]
    pub struct TrayNotifier;

    impl TrayNotifier {
        pub fn balloon(&self, _title: &str, _text: &str, _tag: u64) {}
        pub fn set_tooltip(&self, _text: &str) {}
    }

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
        pub fn notifier(&self) -> Option<TrayNotifier> {
            None
        }
        pub fn notify(&self, _title: &str, _text: &str) {}
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
        tray.post(TrayEvent::Refresh);
        tray.post(TrayEvent::NotificationClicked(7));
        tray.post(TrayEvent::Exit);
        pump_pending();
        assert_eq!(
            *seen.borrow(),
            [
                TrayEvent::Open,
                TrayEvent::Refresh,
                TrayEvent::NotificationClicked(7),
                TrayEvent::Exit
            ]
        );
        // No icon, no notifications: nothing can appear in the notification area.
        assert!(tray.notifier().is_none());
        drop(tray);
        // A dropped tray's window is gone: nothing is delivered any more.
        pump_pending();
        assert_eq!(seen.borrow().len(), 4);
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
