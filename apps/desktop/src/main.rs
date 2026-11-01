//! DsMini desktop client (S0: native window shell).
//!
//! Command line (all optional):
//!   `--smoke-exit-after-ms <N>` — benchmark/smoke mode: print machine-readable
//!       markers on stdout and quit after N ms:
//!       `DSMINI_FIRST_FRAME <ms since main> <renderer>` — after the first painted frame;
//!       `DSMINI_EXIT` — right before the process exits.
//!   `--window-pos <X>,<Y>` — initial outer position in physical pixels
//!       (e.g. place the window on a secondary monitor during tests).
//!   `--no-activate` — create the window without activating it, so it does not
//!       take keyboard focus from the user's current window.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::winit_030::winit::dpi::PhysicalPosition;
use slint::winit_030::winit::event::WindowEvent;
use slint::winit_030::winit::event_loop::ActiveEventLoop;
use slint::winit_030::winit::window::{Window as WinitWindow, WindowId};
use slint::winit_030::{CustomApplicationHandler, EventResult};

slint::include_modules!();

#[derive(Default)]
struct Args {
    smoke_exit_after_ms: Option<u64>,
    window_pos: Option<(i32, i32)>,
    no_activate: bool,
}

fn parse_args() -> Args {
    let mut out = Args::default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--smoke-exit-after-ms" => {
                out.smoke_exit_after_ms = args.next().and_then(|v| v.parse().ok());
            }
            "--window-pos" => {
                out.window_pos = args.next().and_then(|v| {
                    let (x, y) = v.split_once(',')?;
                    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
                });
            }
            "--no-activate" => out.no_activate = true,
            _ => {}
        }
    }
    out
}

/// Reports the first painted frame: `about_to_wait` runs after every event of
/// a loop iteration has been handled, i.e. after the first `RedrawRequested`
/// has been rendered. Works for every renderer (the Slint rendering notifier
/// is unavailable for the software renderer).
struct FirstFrameProbe {
    started: Instant,
    redraw_seen: bool,
    reported: bool,
    renderer: Rc<Cell<&'static str>>,
}

impl CustomApplicationHandler for FirstFrameProbe {
    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        _winit_window: Option<&WinitWindow>,
        _slint_window: Option<&slint::Window>,
        event: &WindowEvent,
    ) -> EventResult {
        if matches!(event, WindowEvent::RedrawRequested) {
            self.redraw_seen = true;
        }
        EventResult::Propagate
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) -> EventResult {
        if self.redraw_seen && !self.reported {
            self.reported = true;
            println!(
                "DSMINI_FIRST_FRAME {} {}",
                self.started.elapsed().as_millis(),
                self.renderer.get()
            );
        }
        EventResult::Propagate
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let started = Instant::now();
    let args = parse_args();
    let smoke = args.smoke_exit_after_ms;
    let renderer = Rc::new(Cell::new("software"));

    // SLINT_BACKEND (e.g. `winit-software`) still selects the renderer.
    let mut selector = slint::BackendSelector::new();
    if args.window_pos.is_some() || args.no_activate {
        let pos = args.window_pos;
        let active = !args.no_activate;
        selector = selector.with_winit_window_attributes_hook(move |attrs| {
            let attrs = attrs.with_active(active);
            match pos {
                Some((x, y)) => attrs.with_position(PhysicalPosition::new(x, y)),
                None => attrs,
            }
        });
    }
    if smoke.is_some() {
        selector = selector.with_winit_custom_application_handler(FirstFrameProbe {
            started,
            redraw_seen: false,
            reported: false,
            renderer: renderer.clone(),
        });
    }
    selector.select()?;

    let window = MainWindow::new()?;
    window.set_build_info(
        format!(
            "dsmini-desktop {} · {} · S0",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS
        )
        .into(),
    );

    if let Some(exit_after) = smoke {
        // GPU renderers expose the graphics API; absence means software rendering.
        let _ = window.window().set_rendering_notifier(move |state, api| {
            if let slint::RenderingState::RenderingSetup = state {
                renderer.set(match api {
                    slint::GraphicsAPI::NativeOpenGL { .. } => "opengl",
                    slint::GraphicsAPI::WebGL { .. } => "webgl",
                    _ => "other-gpu",
                });
            }
        });
        slint::Timer::single_shot(Duration::from_millis(exit_after), || {
            let _ = slint::quit_event_loop();
        });
    }

    window.run()?;
    if smoke.is_some() {
        println!("DSMINI_EXIT");
    }
    Ok(())
}
