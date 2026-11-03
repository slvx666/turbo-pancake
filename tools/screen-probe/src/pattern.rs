//! Moving test pattern on the captured monitor, so Windows.Graphics.Capture delivers a new
//! frame every refresh. The window is never activated (WS_EX_NOACTIVATE, SW_SHOWNOACTIVATE)
//! and never topmost: it must not take focus or input from whoever uses the machine.

#![allow(unsafe_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;

use anyhow::{Result, anyhow};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, FillRect, GetDC, HGDIOBJ, ReleaseDC,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

pub struct Pattern {
    stop: Arc<AtomicBool>,
    frames: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl Pattern {
    /// Opens a `width`×`height` window at (`x`, `y`) and animates it once per compositor
    /// frame until dropped.
    pub fn start(x: i32, y: i32, width: i32, height: i32) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let frames = Arc::new(AtomicU64::new(0));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (thread_stop, thread_frames) = (stop.clone(), frames.clone());
        let thread = std::thread::spawn(move || {
            let result = window_loop(x, y, width, height, &thread_stop, &thread_frames, &ready_tx);
            if let Err(e) = result {
                let _ = ready_tx.send(Err(e));
            }
        });
        ready_rx
            .recv()
            .map_err(|_| anyhow!("pattern thread exited"))??;
        Ok(Self {
            stop,
            frames,
            thread: Some(thread),
        })
    }

    pub fn frames_drawn(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }
}

impl Drop for Pattern {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_MOUSEACTIVATE {
        return LRESULT(MA_NOACTIVATE as isize);
    }
    // SAFETY: default handling for every other message.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn window_loop(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    stop: &AtomicBool,
    frames: &AtomicU64,
    ready: &std::sync::mpsc::Sender<Result<()>>,
) -> Result<()> {
    // SAFETY: plain Win32 window on this thread; every handle is used only here and destroyed
    // before return.
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("TurboPancakeScreenProbePattern"),
            ..Default::default()
        };
        RegisterClassW(&class);
        let hwnd = CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            w!("TurboPancakeScreenProbePattern"),
            w!("screen-probe pattern"),
            WS_POPUP,
            x,
            y,
            width,
            height,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = ready.send(Ok(()));

        let background = CreateSolidBrush(COLORREF(0x0020_2020));
        let bar = CreateSolidBrush(COLORREF(0x00F0_F0F0));
        let full = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        let bar_w = (width / 16).max(8);
        let mut n: i64 = 0;
        let mut msg = MSG::default();
        while !stop.load(Ordering::Relaxed) {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                DispatchMessageW(&msg);
            }
            let dc = GetDC(Some(hwnd));
            let pos = ((n * 12) % i64::from(width.max(1))) as i32;
            FillRect(dc, &full, background);
            let moving = RECT {
                left: pos,
                top: 0,
                right: pos + bar_w,
                bottom: height,
            };
            FillRect(dc, &moving, bar);
            ReleaseDC(Some(hwnd), dc);
            frames.fetch_add(1, Ordering::Relaxed);
            n += 1;
            // One draw per composition pass (the monitor's refresh rate).
            if DwmFlush().is_err() {
                std::thread::sleep(std::time::Duration::from_millis(16));
            }
        }
        let _ = DeleteObject(HGDIOBJ(background.0));
        let _ = DeleteObject(HGDIOBJ(bar.0));
        let _ = DestroyWindow(hwnd);
    }
    Ok(())
}
