//! turbo-pancake desktop client (native window + group voice room).
//!
//! Command line (all optional):
//!   `--server <url>` — signaling endpoint (default `ws://127.0.0.1:7878/v1/signal`).
//!   `--name <display name>` — shown to the other member (default: OS user name).
//!   `--room <id>` — join this room right after start (scripted trials without input).
//!   `--mute` — start with the microphone muted.
//!   `--remote-volume <0..200>` — playback volume of the call in percent.
//!   `--voice-gate <dB>` — start with voice activation at this threshold (dBFS, -60..0).
//!   `--call-log` — print a `TURBO_PANCAKE_CALL key=value …` status line every second.
//!   `--media <auto|udp|tcp>` — how call media reaches the server: UDP with a fallback
//!       to the WebSocket (default), UDP only, or always through the WebSocket (TCP).
//!   `--test-block-udp` — test impairment: drop every UDP media datagram (a VPN that
//!       carries no UDP); with `--media auto` the call must move to the WebSocket.
//!   `--test-send-loss <0..1>` — test impairment: drop this share of outgoing media.
//!   `--test-recv-loss <0..1>` — test impairment: drop this share of incoming media.
//!   `--share <N|synthetic>` — share monitor N (index in the monitor list) or a generated
//!       test picture once in the room (scripted S3 trials).
//!   `--share-quality <qhd120|hd60|hd30>` — at most 1440p120 (20 Mbit/s), 1080p60
//!       (8 Mbit/s, default) or 720p30; fitted to the monitor (no upscaling, no more
//!       frames than its refresh rate).
//!   `--fixed-quality` — keep the chosen quality (no adaptation to the network).
//!   `--watch` — watch the first share that appears.
//!   `--watch-layer <auto|high|low>` — the layer to ask for when watching (default auto).
//!   `--max-previews <N>` — previews received at once, one page of the grid (1–50, 24).
//!   `--no-previews` — start with previews hidden (none received).
//!   `--share-window` — show the watched share in its own window (`--fullscreen`: full
//!       screen on the monitor of that window).
//!   `--share-no-audio` — share without the computer's sound.
//!   `--smoke-exit-after-ms <N>` — benchmark/smoke mode: print machine-readable
//!       markers on stdout and quit after N ms:
//!       `TURBO_PANCAKE_FIRST_FRAME <ms since main> <renderer>` — after the first painted frame;
//!       `TURBO_PANCAKE_EXIT` — right before the process exits.
//!   `--window-pos <X>,<Y>` — initial outer position in physical pixels
//!       (e.g. place the window on a secondary monitor during tests).
//!   `--no-activate` — create the window without activating it, so it does not
//!       take keyboard focus from the user's current window.
//!   `--server-ca <pem>` — trust only this CA for `wss://` / `https://` (a server with a
//!       private certificate, e.g. reached by IP address; `gen-cert` of the server makes
//!       it). Without it the public web PKI is used. Certificates are always verified.
//!   Messenger (S5), the server's HTTP API is derived from `--server`:
//!   `--profile <name>` — local profile: its own stored session and cache (several
//!       clients on one machine, default `default`).
//!   `--login <username>` — sign in at start (password from `TURBO_PANCAKE_PASSWORD`,
//!       never on the command line); `--register` creates the account instead.
//!   `--chat-with <username>` — open the direct conversation with this user.
//!   `--send <text>` — send this message once the conversation is open (repeatable).
//!   `--block` — block the `--chat-with` user once the conversation is open.
//!   `--chat-log` — print `TURBO_PANCAKE_CHAT …` lines on every messenger update.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::Model;
use slint::winit_030::winit::dpi::PhysicalPosition;
use slint::winit_030::winit::event::WindowEvent;
use slint::winit_030::winit::event_loop::ActiveEventLoop;
use slint::winit_030::winit::window::{Window as WinitWindow, WindowId};
use slint::winit_030::{CustomApplicationHandler, EventResult};
use turbo_pancake_desktop::screen::PictureSlot;
use turbo_pancake_desktop::screen::PreviewSlots;
use turbo_pancake_desktop::session::{
    Command, CommandSender, Session, SessionConfig, Snapshot, TransportPref, WatchLayer,
};
use turbo_pancake_desktop::view::{self, MemberView, ShareRowView};
use turbo_pancake_media_audio::{CpalIo, DspConfig, device};
use turbo_pancake_media_video::{MonitorInfo, ShareConfig, ShareSource};
use turbo_pancake_protocol::validate_display_name;

slint::include_modules!();

mod messenger_ui;

#[derive(Default)]
struct Args {
    simulate_audio: bool,
    snapshot: Option<String>,
    snapshot_after_ms: Option<u64>,
    smoke_exit_after_ms: Option<u64>,
    window_pos: Option<(i32, i32)>,
    no_activate: bool,
    server: Option<String>,
    name: Option<String>,
    room: Option<String>,
    mute: bool,
    /// Percent, 0–200.
    remote_volume: Option<f32>,
    voice_gate: Option<f32>,
    call_log: bool,
    send_loss: f32,
    recv_loss: f32,
    media: Option<TransportPref>,
    block_udp: bool,
    share: Option<ShareSource>,
    /// 0 = 1440p120, 1 = 1080p60, 2 = 720p30.
    share_quality: i32,
    fixed_quality: bool,
    watch: bool,
    watch_layer: Option<WatchLayer>,
    max_previews: Option<usize>,
    no_previews: bool,
    share_window: bool,
    fullscreen: bool,
    share_no_audio: bool,
    chat: messenger_ui::Script,
    server_ca: Option<String>,
}

fn parse_args() -> Args {
    let mut out = Args {
        share_quality: 1,
        ..Args::default()
    };
    out.chat.profile = "default".into();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--simulate-audio" => out.simulate_audio = true,
            "--trial-chat-switch" => out.chat.trial_switch = true,
            "--snapshot" => out.snapshot = args.next(),
            "--snapshot-after-ms" => {
                out.snapshot_after_ms = args.next().and_then(|v| v.parse().ok())
            }
            "--group-title" => out.chat.group_title = args.next(),
            "--group-members" => {
                out.chat.group_members = args
                    .next()
                    .unwrap_or_default()
                    .split(',')
                    .map(str::to_owned)
                    .collect()
            }
            "--open-group" => out.chat.open_group = args.next(),
            "--call" => out.chat.call = true,
            "--auto-join-call" => out.chat.auto_join = true,
            "--auto-decline-call" => out.chat.auto_decline = true,
            "--leave-call-after-ms" => {
                out.chat.leave_after_ms = args.next().and_then(|v| v.parse().ok())
            }
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
            "--remote-volume" => {
                out.remote_volume = args.next().and_then(|v| v.parse().ok());
            }
            "--voice-gate" => {
                out.voice_gate = args
                    .next()
                    .and_then(|v| v.parse::<f32>().ok())
                    .map(|v| v.clamp(view::METER_FLOOR_DB, 0.0));
            }
            "--call-log" => out.call_log = true,
            "--media" => {
                out.media = match args.next().as_deref() {
                    Some("udp") => Some(TransportPref::Udp),
                    Some("tcp" | "websocket") => Some(TransportPref::Tunnel),
                    _ => Some(TransportPref::Auto),
                };
            }
            "--test-block-udp" => out.block_udp = true,
            "--share" => {
                out.share = match args.next().as_deref() {
                    Some("synthetic") => Some(ShareSource::Synthetic),
                    Some(n) => n.parse().ok().map(ShareSource::Monitor),
                    None => None,
                };
            }
            "--share-quality" => {
                out.share_quality = match args.next().as_deref() {
                    Some("qhd120") => 0,
                    Some("hd30") => 2,
                    _ => 1,
                };
            }
            "--fixed-quality" => out.fixed_quality = true,
            "--watch" => out.watch = true,
            "--watch-layer" => {
                out.watch_layer = match args.next().as_deref() {
                    Some("high") => Some(WatchLayer::High),
                    Some("low") => Some(WatchLayer::Low),
                    _ => Some(WatchLayer::Auto),
                };
            }
            "--max-previews" => {
                out.max_previews = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .map(|n: usize| n.clamp(1, turbo_pancake_protocol::MAX_PREVIEWS));
            }
            "--no-previews" => out.no_previews = true,
            "--share-window" => out.share_window = true,
            "--fullscreen" => out.fullscreen = true,
            "--share-no-audio" => out.share_no_audio = true,
            "--server-ca" => out.server_ca = args.next(),
            "--profile" => {
                if let Some(p) = args.next().filter(|p| {
                    !p.is_empty()
                        && p.len() <= 32
                        && p.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                }) {
                    out.chat.profile = p;
                }
            }
            "--login" => out.chat.login = args.next(),
            "--register" => out.chat.register = true,
            "--chat-with" => out.chat.chat_with = args.next(),
            "--send" => out.chat.send.extend(args.next()),
            "--chat-log" => out.chat.chat_log = true,
            "--block" => out.chat.block = true,
            "--test-recv-loss" => {
                out.recv_loss = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0.0f32)
                    .clamp(0.0, 1.0);
            }
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

/// Updates the member list in place, so a slider being dragged is not rebuilt every
/// status update; the list is replaced only when members come or go.
fn show_members(w: &MainWindow, members: Vec<MemberView>) {
    let rows: Vec<MemberRow> = members
        .into_iter()
        .map(|m| MemberRow {
            peer_id: m.peer_id.into(),
            name: m.name.into(),
            speaking: m.speaking,
            volume: m.volume_pct,
            info: m.info.into(),
        })
        .collect();
    let model = w.get_members();
    let Some(list) = model.as_any().downcast_ref::<slint::VecModel<MemberRow>>() else {
        w.set_members(slint::ModelRc::new(slint::VecModel::from(rows)));
        return;
    };
    let same_people = list.row_count() == rows.len()
        && rows
            .iter()
            .enumerate()
            .all(|(i, r)| list.row_data(i).is_some_and(|o| o.peer_id == r.peer_id));
    if !same_people {
        list.set_vec(rows);
        return;
    }
    for (i, r) in rows.into_iter().enumerate() {
        if list.row_data(i).as_ref() != Some(&r) {
            list.set_row_data(i, r);
        }
    }
}

/// Updates the share list; a row keeps its preview picture while its preview stream
/// stays the same (pictures arrive through [`show_pictures`]).
fn show_shares(w: &MainWindow, shares: Vec<ShareRowView>) {
    let model = w.get_shares();
    let list = model.as_any().downcast_ref::<slint::VecModel<ShareRow>>();
    let old: Vec<ShareRow> = list.map(|l| l.iter().collect()).unwrap_or_default();
    let rows: Vec<ShareRow> = shares
        .into_iter()
        .map(|s| {
            let ssrc = s.preview_ssrc.map_or(0, |v| v as i32);
            let kept = old
                .iter()
                .find(|o| o.peer_id.as_str() == s.peer_id && o.preview_ssrc == ssrc && ssrc != 0);
            ShareRow {
                peer_id: s.peer_id.into(),
                name: s.name.into(),
                info: s.info.into(),
                watching: s.watching,
                preview: kept.map(|k| k.preview.clone()).unwrap_or_default(),
                has_preview: kept.is_some_and(|k| k.has_preview),
                preview_ssrc: ssrc,
            }
        })
        .collect();
    match list {
        Some(list) if list.iter().eq(rows.iter().cloned()) => {}
        Some(list) if list.row_count() == rows.len() => {
            for (i, r) in rows.into_iter().enumerate() {
                if list.row_data(i).as_ref() != Some(&r) {
                    list.set_row_data(i, r);
                }
            }
        }
        Some(list) => list.set_vec(rows),
        None => w.set_shares(slint::ModelRc::new(slint::VecModel::from(rows))),
    }
}

fn monitor_label(m: &MonitorInfo) -> String {
    format!(
        "Монитор {} — {}×{} @ {} Гц{}",
        m.index + 1,
        m.width,
        m.height,
        m.refresh_hz,
        if m.primary { " (основной)" } else { "" }
    )
}

/// The share settings for a quality index (0 = 1440p120, 1 = 1080p60, 2 = 720p30),
/// fitted to the monitor.
fn share_config(
    source: ShareSource,
    quality: i32,
    adaptive: bool,
    monitors: &[MonitorInfo],
) -> ShareConfig {
    let mut c = match quality {
        0 => ShareConfig::qhd120(source),
        2 => ShareConfig::hd30(source),
        _ => ShareConfig::hd60(source),
    };
    c.adaptive = adaptive;
    match source {
        ShareSource::Monitor(i) => match monitors.iter().find(|m| m.index == i) {
            Some(m) => c.fitted((m.width, m.height), m.refresh_hz),
            None => c,
        },
        ShareSource::Synthetic => c,
    }
}

/// The watched share in its own window (spec §5.1), created on demand.
type ShareWindowSlot = Rc<std::cell::RefCell<Option<ShareWindow>>>;

fn open_share_window(main: &MainWindow, slot: &ShareWindowSlot, fullscreen: bool) {
    let mut cell = slot.borrow_mut();
    if cell.is_none() {
        let Ok(win) = ShareWindow::new() else {
            return;
        };
        let (weak_main, weak_slot) = (main.as_weak(), Rc::downgrade(slot));
        let close = move || {
            if let Some(slot) = weak_slot.upgrade()
                && let Some(w) = slot.borrow_mut().take()
            {
                let _ = w.hide();
            }
            if let Some(m) = weak_main.upgrade() {
                m.set_share_window_open(false);
            }
        };
        let close2 = close.clone();
        win.on_close_window(close);
        win.window().on_close_requested(move || {
            close2();
            slint::CloseRequestResponse::HideWindow
        });
        let weak = win.as_weak();
        win.on_fullscreen_changed(move |on| {
            if let Some(w) = weak.upgrade() {
                w.window().set_fullscreen(on);
            }
        });
        if win.show().is_err() {
            return;
        }
        *cell = Some(win);
    }
    if let Some(w) = cell.as_ref() {
        w.set_fullscreen(fullscreen);
        w.window().set_fullscreen(fullscreen);
        w.set_share_name(main.get_watching_name());
    }
    main.set_share_window_open(true);
}

/// Shows the newest picture of the watched share (UI thread, ~60 times a second; the
/// viewer thread decodes and converts, the UI only uploads).
fn show_pictures(
    window: &MainWindow,
    slot: PictureSlot,
    previews: PreviewSlots,
    share_window: ShareWindowSlot,
) -> slint::Timer {
    let timer = slint::Timer::default();
    let weak = window.as_weak();
    let mut last_serial = 0;
    let mut preview_serials: std::collections::HashMap<i32, u64> = Default::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(16),
        move || {
            let Some(w) = weak.upgrade() else {
                return;
            };
            let picture = slot.lock().unwrap().take();
            let sw = share_window.borrow();
            match picture {
                Some(p) if p.serial != last_serial => {
                    last_serial = p.serial;
                    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                        &p.rgba, p.width, p.height,
                    );
                    let image = slint::Image::from_rgba8(buffer);
                    if let Some(sw) = sw.as_ref() {
                        sw.set_picture(image.clone());
                        sw.set_has_picture(true);
                        sw.set_info(w.get_viewer_info());
                    }
                    w.set_screen_picture(image);
                    w.set_has_picture(true);
                }
                Some(_) => {}
                None if w.get_watching().is_empty() && w.get_has_picture() => {
                    w.set_has_picture(false);
                    w.set_screen_picture(slint::Image::default());
                }
                None => {}
            }
            drop(sw);
            // The watched share ended: its window closes.
            if w.get_watching().is_empty() && w.get_share_window_open() {
                if let Some(sw) = share_window.borrow_mut().take() {
                    let _ = sw.hide();
                }
                w.set_share_window_open(false);
            }
            // New preview pictures go into their rows.
            let model = w.get_shares();
            if let Some(list) = model.as_any().downcast_ref::<slint::VecModel<ShareRow>>() {
                let slots = previews.lock().unwrap();
                for i in 0..list.row_count() {
                    let Some(mut row) = list.row_data(i) else {
                        continue;
                    };
                    if row.preview_ssrc == 0 {
                        continue;
                    }
                    let Some(p) = slots.get(&(row.preview_ssrc as u32)) else {
                        continue;
                    };
                    if preview_serials.get(&row.preview_ssrc) == Some(&p.serial) {
                        continue;
                    }
                    preview_serials.insert(row.preview_ssrc, p.serial);
                    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                        &p.rgba, p.width, p.height,
                    );
                    row.preview = slint::Image::from_rgba8(buffer);
                    row.has_preview = true;
                    list.set_row_data(i, row);
                }
            }
        },
    );
    timer
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
/// Trust for `wss://` / `https://`: the CA from `--server-ca`, else the web PKI.
fn client_tls(args: &Args) -> Result<turbo_pancake_messenger_client::api::TlsConfig, String> {
    match &args.server_ca {
        Some(path) => {
            let pem = std::fs::read(path).map_err(|e| format!("--server-ca: {e}"))?;
            turbo_pancake_tls::client_config(Some(&pem)).map_err(|e| format!("--server-ca: {e}"))
        }
        None => Ok(turbo_pancake_messenger_client::api::web_pki()),
    }
}

fn start_session(
    window: &MainWindow,
    args: &Args,
    started: Instant,
    tls: Option<turbo_pancake_messenger_client::api::TlsConfig>,
) -> Option<Session> {
    let name = args.name.clone().unwrap_or_else(default_name);
    window.set_user_name(name.as_str().into());
    let server = args
        .server
        .clone()
        .unwrap_or_else(|| "ws://127.0.0.1:7878/v1/signal".to_string());
    let mut cfg = SessionConfig::new(server, name);
    cfg.tls = tls;
    cfg.engine.muted = args.mute;
    cfg.engine.voice_gate = args.voice_gate;
    cfg.send_loss = args.send_loss;
    cfg.recv_loss = args.recv_loss;
    cfg.transport = args.media.unwrap_or(TransportPref::Auto);
    cfg.block_udp = args.block_udp;
    cfg.share_audio = !args.share_no_audio;
    if let Some(n) = args.max_previews {
        cfg.max_previews = n;
    }
    window.set_share_audio(cfg.share_audio);
    window.set_muted(args.mute);
    window.set_ns(cfg.engine.dsp.noise_suppression);
    window.set_aec(cfg.engine.dsp.echo_cancellation);
    window.set_voice_activation(args.voice_gate.is_some());
    if let Some(g) = args.voice_gate {
        window.set_vad_threshold(g);
    }
    window.set_members(slint::ModelRc::new(slint::VecModel::<MemberRow>::default()));

    let monitors = turbo_pancake_media_video::monitors();
    let labels: Vec<slint::SharedString> =
        monitors.iter().map(|m| monitor_label(m).into()).collect();
    window.set_monitors(slint::ModelRc::new(slint::VecModel::from(labels)));
    window.set_shares(slint::ModelRc::new(slint::VecModel::<ShareRow>::default()));
    window.set_quality_index(args.share_quality);
    window.set_adaptive(!args.fixed_quality);
    window.set_previews_visible(!args.no_previews);
    window.set_layer_index(match args.watch_layer {
        Some(WatchLayer::High) => 1,
        Some(WatchLayer::Low) => 2,
        _ => 0,
    });

    let weak = window.as_weak();
    let call_log = args.call_log;
    let mut last_log: Option<Instant> = None;
    // `--watch`: the session's own sender, known once it started.
    let auto_watch: Arc<Mutex<Option<CommandSender>>> = Arc::default();
    let watcher = auto_watch.clone();
    let on_update = move |s: &Snapshot| {
        if call_log && last_log.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
            last_log = Some(Instant::now());
            println!("{}", view::log_line(s, started.elapsed().as_millis()));
        }
        if s.watching.is_none()
            && let Some(first) = s.shares.first()
            && let Some(tx) = watcher.lock().unwrap().take()
        {
            tx.send(Command::Watch(Some(first.peer_id.clone())));
        }
        let v = view::render(s);
        let (room, error) = (s.room.clone(), s.last_error.clone());
        let has_room = room.is_some();
        let _ = weak.upgrade_in_event_loop(move |w| {
            messenger_ui::media_state(&w, room, error);
            // Signed in, the sidebar shows the messenger connection instead.
            if !w.get_signed_in() {
                w.set_connection_status(v.connection.into());
            }
            w.set_in_room(v.in_room);
            w.set_call_title(v.title.into());
            w.set_call_detail(if w.get_signed_in() && !has_room {
                "Начните звонок из личного или группового диалога".into()
            } else {
                v.detail.into()
            });
            w.set_call_stats(v.stats.into());
            w.set_call_devices(v.devices.into());
            w.set_call_error(v.error.into());
            w.set_mic_level(v.mic_level);
            w.set_self_speaking(v.speaking);
            w.set_dsp_available(v.dsp_available);
            show_members(&w, v.members);
            w.set_sharing(v.sharing);
            w.set_share_info(v.share_info.into());
            let watched_name = v
                .shares
                .iter()
                .find(|s| s.watching)
                .map(|s| s.name.clone())
                .unwrap_or_default();
            w.set_watching_name(watched_name.into());
            w.set_watching(v.watching.into());
            w.set_viewer_info(v.viewer_info.into());
            w.set_layer_info(v.layer_info.into());
            w.set_preview_info(v.preview_info.into());
            w.set_preview_page(v.preview_page as i32);
            w.set_preview_pages(v.preview_pages as i32);
            show_shares(&w, v.shares);
        });
    };
    let started_session = if args.simulate_audio {
        Session::start(
            cfg,
            || {
                use turbo_pancake_media_audio::sim::{SimDevice, SimIo};
                SimIo::new(
                    vec![SimDevice::new("simulated mic", 48_000)],
                    vec![SimDevice::new("simulated speaker", 48_000)],
                )
            },
            on_update,
        )
    } else {
        Session::start(cfg, || CpalIo, on_update)
    };
    let session = match started_session {
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
    let tx = session.sender();
    window.on_member_volume_changed(move |peer, v| {
        tx.send(Command::SetMemberVolume(peer.to_string(), v / 100.0));
    });
    let tx = session.sender();
    window.on_vad_changed(move |on, threshold| {
        tx.send(Command::SetVoiceGate(on.then_some(threshold)));
    });
    let tx = session.sender();
    let indexes: Vec<usize> = monitors.iter().map(|m| m.index).collect();
    let list = monitors.clone();
    window.on_start_share(move |monitor, quality, adaptive| {
        let Some(index) = usize::try_from(monitor).ok().and_then(|i| indexes.get(i)) else {
            return;
        };
        tx.send(Command::StartShare(share_config(
            ShareSource::Monitor(*index),
            quality,
            adaptive,
            &list,
        )));
    });
    let tx = session.sender();
    window.on_layer_selected(move |i| {
        tx.send(Command::SetWatchLayer(match i {
            1 => WatchLayer::High,
            2 => WatchLayer::Low,
            _ => WatchLayer::Auto,
        }));
    });
    let tx = session.sender();
    let per_page = args
        .max_previews
        .unwrap_or(SessionConfig::new("", "").max_previews);
    window.on_previews_changed(move |visible, page| {
        tx.send(Command::SetPreviews {
            visible,
            first: usize::try_from(page).unwrap_or(0) * per_page,
        });
    });
    let tx = session.sender();
    window.on_stop_share(move || {
        tx.send(Command::StopShare);
    });
    let tx = session.sender();
    window.on_share_audio_changed(move |on| {
        tx.send(Command::SetShareAudio(on));
    });
    let tx = session.sender();
    window.on_watch(move |peer| {
        let peer = peer.to_string();
        tx.send(Command::Watch((!peer.is_empty()).then_some(peer)));
    });
    if args.watch {
        *auto_watch.lock().unwrap() = Some(session.sender());
    }

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

    if let Some(v) = args.remote_volume {
        let v = v.clamp(0.0, 200.0);
        window.set_remote_volume(v);
        session.send(Command::SetRemoteVolume(v / 100.0));
    }
    if let Some(room) = &args.room {
        window.set_room_text(room.as_str().into());
        session.send(Command::Join(room.clone()));
    }
    if let Some(source) = args.share {
        if let ShareSource::Monitor(i) = source
            && let Some(pos) = monitors.iter().position(|m| m.index == i)
        {
            window.set_monitor_index(pos as i32);
        }
        session.send(Command::StartShare(share_config(
            source,
            args.share_quality,
            !args.fixed_quality,
            &monitors,
        )));
    }
    if let Some(layer) = args.watch_layer {
        session.send(Command::SetWatchLayer(layer));
    }
    if args.no_previews {
        session.send(Command::SetPreviews {
            visible: false,
            first: 0,
        });
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
            "turbo-pancake-desktop {} · {} · S5",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS
        )
        .into(),
    );
    let tls = match client_tls(&args) {
        Ok(t) => Some(t),
        Err(e) => {
            // A broken trust file must not fall back to anything: nothing connects.
            window.set_call_error(e.clone().into());
            window.set_auth_error(e.into());
            None
        }
    };
    let session = tls
        .clone()
        .and_then(|t| start_session(&window, &args, started, Some(t)));
    // A scripted call opens on the voice room; otherwise sign-in / conversations first.
    window.set_voice_selected(args.room.is_some() || args.share.is_some());
    let server = args
        .server
        .clone()
        .unwrap_or_else(|| "ws://127.0.0.1:7878/v1/signal".to_string());
    let messenger = tls.map(|t| {
        messenger_ui::start(
            &window,
            &server,
            args.chat.clone(),
            t,
            session.as_ref().map(Session::sender),
        )
    });
    let share_window: ShareWindowSlot = Rc::default();
    let _pictures = session
        .as_ref()
        .map(|s| show_pictures(&window, s.picture(), s.previews(), share_window.clone()));
    {
        let (weak, slot) = (window.as_weak(), share_window.clone());
        window.on_open_share_window(move |fullscreen| {
            if let Some(w) = weak.upgrade() {
                open_share_window(&w, &slot, fullscreen);
            }
        });
    }
    // `--share-window`: open it as soon as something is watched.
    let _auto_window = args.share_window.then(|| {
        let timer = slint::Timer::default();
        let (weak, slot, fullscreen) = (window.as_weak(), share_window.clone(), args.fullscreen);
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(250),
            move || {
                if let Some(w) = weak.upgrade()
                    && !w.get_watching().is_empty()
                    && !w.get_share_window_open()
                {
                    open_share_window(&w, &slot, fullscreen);
                }
            },
        );
        timer
    });

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

    if let Some(path) = args.snapshot {
        let weak = window.as_weak();
        slint::Timer::single_shot(
            Duration::from_millis(args.snapshot_after_ms.unwrap_or(8000)),
            move || {
                if let Some(w) = weak.upgrade() {
                    match w.window().take_snapshot() {
                        Ok(pixels) => {
                            use std::io::Write;
                            let result = (|| -> std::io::Result<()> {
                                let mut file =
                                    std::io::BufWriter::new(std::fs::File::create(&path)?);
                                write!(file, "P6\n{} {}\n255\n", pixels.width(), pixels.height())?;
                                for p in pixels.as_slice() {
                                    file.write_all(&[p.r, p.g, p.b])?;
                                }
                                file.flush()?;
                                Ok(())
                            })();
                            println!("TURBO_PANCAKE_SNAPSHOT success={}", result.is_ok());
                        }
                        Err(_) => println!("TURBO_PANCAKE_SNAPSHOT success=false"),
                    }
                }
            },
        );
    }
    window.run()?;
    if let Some(w) = share_window.borrow_mut().take() {
        let _ = w.hide();
    }
    // Leave the room and release audio devices before the process exits.
    drop(window);
    if let Some(m) = messenger {
        m.stop();
    }
    if let Some(s) = session {
        s.stop();
    }
    if smoke.is_some() {
        println!("TURBO_PANCAKE_EXIT");
    }
    Ok(())
}
