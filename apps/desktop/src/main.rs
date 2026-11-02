//! turbo-pancake desktop client (S1: native window + 1:1 voice room).
//!
//! Command line (all optional):
//!   `--server <url>` — signaling endpoint (default `ws://127.0.0.1:7878/v1/signal`).
//!   `--name <display name>` — shown to the other member (default: OS user name).
//!   `--room <id>` — join this room right after start (scripted trials without input).
//!   `--mute` — start with the microphone muted.
//!   `--call-log` — print a `TURBO_PANCAKE_CALL key=value …` status line every second.
//!   `--test-send-loss <0..1>` — test impairment: drop this share of outgoing media.
//!   `--smoke-exit-after-ms <N>` — benchmark/smoke mode: print machine-readable
//!       markers on stdout and quit after N ms:
//!       `TURBO_PANCAKE_FIRST_FRAME <ms since main> <renderer>` — after the first painted frame;
//!       `TURBO_PANCAKE_EXIT` — right before the process exits.
//!   `--window-pos <X>,<Y>` — initial outer position in physical pixels
//!       (e.g. place the window on a secondary monitor during tests).
//!   `--no-activate` — create the window without activating it, so it does not
//!       take keyboard focus from the user's current window.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::winit_030::winit::dpi::PhysicalPosition;
use slint::winit_030::winit::event::WindowEvent;
use slint::winit_030::winit::event_loop::ActiveEventLoop;
use slint::winit_030::winit::window::{Window as WinitWindow, WindowId};
use slint::winit_030::{CustomApplicationHandler, EventResult};
use turbo_pancake_desktop::session::{Command, Session, SessionConfig, Snapshot};
use turbo_pancake_desktop::view;
use turbo_pancake_media_audio::{CpalIo, DspConfig, device};
use turbo_pancake_protocol::validate_display_name;

slint::include_modules!();

#[derive(Default)]
struct Args {
    smoke_exit_after_ms: Option<u64>,
    window_pos: Option<(i32, i32)>,
    no_activate: bool,
    server: Option<String>,
    name: Option<String>,
    room: Option<String>,
    mute: bool,
    call_log: bool,
    send_loss: f32,
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
            "--server" => out.server = args.next(),
            "--name" => out.name = args.next(),
            "--room" => out.room = args.next(),
            "--mute" => out.mute = true,
            "--call-log" => out.call_log = true,
            "--test-send-loss" => {
                out.send_loss = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0.0f32)
                    .clamp(0.0, 1.0);
            }
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
                "TURBO_PANCAKE_FIRST_FRAME {} {}",
                self.started.elapsed().as_millis(),
                self.renderer.get()
            );
        }
        EventResult::Propagate
    }
}

/// Device ids behind the combo box entries (index 0 = system default).
#[derive(Default)]
struct DeviceIds {
    inputs: Vec<String>,
    outputs: Vec<String>,
}

fn pick(ids: &[String], index: i32) -> Option<String> {
    usize::try_from(index)
        .ok()
        .and_then(|i| i.checked_sub(1))
        .and_then(|i| ids.get(i).cloned())
}

fn default_name() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .ok()
        .filter(|n| validate_display_name(n).is_ok())
        .unwrap_or_else(|| "Гость".to_string())
}

/// Starts the session thread and wires it to the window: commands from callbacks,
/// snapshots into properties (via the event loop), device lists from a helper thread.
fn start_session(window: &MainWindow, args: &Args, started: Instant) -> Option<Session> {
    let name = args.name.clone().unwrap_or_else(default_name);
    window.set_user_name(name.as_str().into());
    let server = args
        .server
        .clone()
        .unwrap_or_else(|| "ws://127.0.0.1:7878/v1/signal".to_string());
    let mut cfg = SessionConfig::new(server, name);
    cfg.engine.muted = args.mute;
    cfg.send_loss = args.send_loss;
    window.set_muted(args.mute);
    window.set_ns(cfg.engine.dsp.noise_suppression);
    window.set_aec(cfg.engine.dsp.echo_cancellation);

    let weak = window.as_weak();
    let call_log = args.call_log;
    let mut last_log: Option<Instant> = None;
    let on_update = move |s: &Snapshot| {
        if call_log && last_log.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
            last_log = Some(Instant::now());
            println!("{}", view::log_line(s, started.elapsed().as_millis()));
        }
        let v = view::render(s);
        let _ = weak.upgrade_in_event_loop(move |w| {
            w.set_connection_status(v.connection.into());
            w.set_in_room(v.in_room);
            w.set_call_title(v.title.into());
            w.set_call_detail(v.detail.into());
            w.set_call_stats(v.stats.into());
            w.set_call_devices(v.devices.into());
            w.set_call_error(v.error.into());
            w.set_mic_level(v.mic_level);
            w.set_dsp_available(v.dsp_available);
        });
    };
    let session = match Session::start(cfg, || CpalIo, on_update) {
        Ok(s) => s,
        Err(e) => {
            window.set_call_error(format!("не удалось запустить сессию: {e}").into());
            return None;
        }
    };

    let ids = Arc::new(Mutex::new(DeviceIds::default()));
    let tx = session.sender();
    window.on_join(move |room| {
        tx.send(Command::Join(room.trim().to_string()));
    });
    let tx = session.sender();
    window.on_leave(move || {
        tx.send(Command::Leave);
    });
    let tx = session.sender();
    window.on_mute_changed(move |m| {
        tx.send(Command::SetMuted(m));
    });
    let tx = session.sender();
    window.on_dsp_changed(move |ns, aec| {
        tx.send(Command::SetDsp(DspConfig {
            noise_suppression: ns,
            echo_cancellation: aec,
            auto_gain: false,
        }));
    });
    let (tx, map) = (session.sender(), ids.clone());
    window.on_input_selected(move |i| {
        tx.send(Command::SetInput(pick(&map.lock().unwrap().inputs, i)));
    });
    let (tx, map) = (session.sender(), ids.clone());
    window.on_output_selected(move |i| {
        tx.send(Command::SetOutput(pick(&map.lock().unwrap().outputs, i)));
    });
    let tx = session.sender();
    window.on_remote_volume_changed(move |v| {
        tx.send(Command::SetRemoteVolume(v / 100.0));
    });

    // Device enumeration can take a while (WASAPI, PulseAudio): keep it off the UI thread.
    let weak = window.as_weak();
    let _ = std::thread::Builder::new()
        .name("tp-devices".into())
        .spawn(move || {
            let list = device::list_devices();
            let names = |v: &[device::DeviceInfo]| {
                std::iter::once("По умолчанию".to_string())
                    .chain(v.iter().map(|d| d.name.clone()))
                    .collect::<Vec<_>>()
            };
            let (inputs, outputs) = (names(&list.inputs), names(&list.outputs));
            *ids.lock().unwrap() = DeviceIds {
                inputs: list.inputs.into_iter().map(|d| d.id).collect(),
                outputs: list.outputs.into_iter().map(|d| d.id).collect(),
            };
            let _ = weak.upgrade_in_event_loop(move |w| {
                let model = |v: Vec<String>| {
                    let v: Vec<slint::SharedString> = v.into_iter().map(Into::into).collect();
                    slint::ModelRc::new(slint::VecModel::from(v))
                };
                w.set_inputs(model(inputs));
                w.set_outputs(model(outputs));
            });
        });

    if let Some(room) = &args.room {
        window.set_room_text(room.as_str().into());
        session.send(Command::Join(room.clone()));
    }
    Some(session)
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
            "turbo-pancake-desktop {} · {} · S1",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS
        )
        .into(),
    );
    let session = start_session(&window, &args, started);

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
    // Leave the room and release audio devices before the process exits.
    drop(window);
    if let Some(s) = session {
        s.stop();
    }
    if smoke.is_some() {
        println!("TURBO_PANCAKE_EXIT");
    }
    Ok(())
}
