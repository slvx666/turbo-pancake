//! Offscreen UI snapshots (dev tool): renders `MainWindow` with Slint's software renderer
//! into PNG files, filled with generic fake data. No window is created, nothing is shown
//! on the desktop and no input is sent to the system.
//!
//! ```text
//! cargo run -p turbo-pancake-desktop --example ui_snapshot -- [out-dir]
//! (default: .local/logs/ui-snapshots under the current directory)
//! ```
//!
//! Every scene is written in dark and light at 1280×760 (plus a narrow 760 px window and
//! one at 150 % scale); the file names are printed.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, WindowAdapter, WindowEvent};
use slint::{
    ComponentHandle, Image, LogicalPosition, ModelRc, PhysicalSize, Rgb8Pixel, SharedPixelBuffer,
    VecModel,
};
use turbo_pancake_desktop::chat;
use turbo_pancake_messenger_client::{MessageState, MessageView, ReplyView};

slint::include_modules!();

thread_local! {
    static WINDOW: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
    static CLOCK: Cell<Duration> = const { Cell::new(Duration::ZERO) };
}

/// A platform without a windowing system: windows are software framebuffers and time
/// moves only when the tool advances it.
struct Offscreen;

impl Platform for Offscreen {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        WINDOW.with_borrow_mut(|w| *w = Some(window.clone()));
        Ok(window)
    }

    fn duration_since_start(&self) -> Duration {
        CLOCK.get()
    }
}

/// Lets timers, change handlers and animations run as if `ms` had passed.
fn advance(ms: u64) {
    for _ in 0..ms / 20 {
        CLOCK.set(CLOCK.get() + Duration::from_millis(20));
        slint::platform::update_timers_and_animations();
    }
}

const NOW_MS: i64 = 1_795_179_600_000; // 2026-11-20 12:00 UTC
const MIN: i64 = 60_000;
const HOUR: i64 = 60 * MIN;
const DAY: i64 = 24 * HOUR;

#[derive(Clone, Copy, PartialEq)]
enum Scene {
    SignIn,
    Empty,
    Chat,
    Group,
    Call,
    CallWaiting,
    CallMuted,
    CallShares,
    CallSettings,
    Incoming,
    SettingsLook,
    SettingsAlerts,
    SettingsVoice,
}

impl Scene {
    fn name(self) -> &'static str {
        match self {
            Scene::SignIn => "signin",
            Scene::Empty => "empty",
            Scene::Chat => "chat",
            Scene::Group => "group",
            Scene::Call => "call",
            Scene::CallWaiting => "call-waiting",
            Scene::CallMuted => "call-muted",
            Scene::CallShares => "call-shares",
            Scene::CallSettings => "call-voice-panel",
            Scene::Incoming => "incoming",
            Scene::SettingsLook => "settings-appearance",
            Scene::SettingsAlerts => "settings-notifications",
            Scene::SettingsVoice => "settings-voice",
        }
    }
}

fn message(
    key: &str,
    author: (&str, &str),
    at: i64,
    body: &str,
    state: MessageState,
) -> MessageView {
    MessageView {
        key: key.into(),
        message_id: key.into(),
        author_id: author.0.into(),
        author_name: author.1.into(),
        mine: author.0 == "u-me",
        body: body.into(),
        reply: None,
        created_at_ms: at,
        edited: false,
        deleted: false,
        state,
        server_sequence: 1,
        pending_nonce: String::new(),
    }
}

fn direct_messages() -> Vec<MessageView> {
    let me = ("u-me", "Алиса");
    let bob = ("u-bob", "Борис");
    let t0 = NOW_MS - 8 * DAY - 3 * HOUR;
    let y = NOW_MS - DAY - 2 * HOUR;
    let t = NOW_MS - 40 * MIN;
    let mut list = vec![
        message(
            "1",
            bob,
            t0,
            "Привет! Как продвигается новый интерфейс?",
            MessageState::Received,
        ),
        message(
            "2",
            me,
            t0 + 4 * MIN,
            "Почти готов: осталось нарисовать окно звонка.",
            MessageState::Read,
        ),
        message(
            "3",
            bob,
            y,
            "Посмотрел сборку — список диалогов стал гораздо аккуратнее.",
            MessageState::Received,
        ),
        message(
            "4",
            bob,
            y + MIN,
            "И тёмная тема приятная, глаза не устают 👍",
            MessageState::Received,
        ),
        message(
            "5",
            me,
            y + 20 * MIN,
            "Спасибо! Сегодня пришлю обновление.",
            MessageState::Read,
        ),
        message(
            "6",
            bob,
            t,
            "Созвонимся вечером, обсудим планы на следующую неделю?",
            MessageState::Received,
        ),
        message(
            "7",
            me,
            t + 2 * MIN,
            "Да, давай в семь. Я заодно покажу демонстрацию экрана.",
            MessageState::Read,
        ),
        message(
            "8",
            me,
            t + 3 * MIN,
            "Это сообщение было удалено",
            MessageState::Read,
        ),
        message(
            "9",
            me,
            t + 30 * MIN,
            "Захвати, пожалуйста, заметки со вчерашней встречи.",
            MessageState::Delivered,
        ),
        message(
            "10",
            me,
            t + 31 * MIN,
            "И ссылку на макеты",
            MessageState::Failed("server unreachable".into()),
        ),
    ];
    list[3].edited = true;
    list[7].deleted = true;
    list[6].reply = Some(ReplyView {
        message_id: "6".into(),
        author_name: "Борис".into(),
        text: "Созвонимся вечером, обсудим планы на следующую неделю?".into(),
        deleted: false,
    });
    list[9].server_sequence = 0;
    list
}

fn group_messages() -> Vec<MessageView> {
    let me = ("u-me", "Алиса");
    let bob = ("u-bob", "Борис");
    let vera = ("u-vera", "Вера Смирнова");
    let gleb = ("u-gleb", "Глеб");
    let t = NOW_MS - 3 * HOUR;
    let mut list = vec![
        message(
            "g1",
            vera,
            t,
            "Всем привет! Сборка 0.1.5 готова к проверке.",
            MessageState::Received,
        ),
        message(
            "g2",
            vera,
            t + MIN,
            "Список изменений в описании релиза.",
            MessageState::Received,
        ),
        message(
            "g3",
            gleb,
            t + 12 * MIN,
            "Проверю звонки на ноутбуке после обеда.",
            MessageState::Received,
        ),
        message(
            "g4",
            bob,
            t + 30 * MIN,
            "У меня всё работает, задержка звука около 40 мс.",
            MessageState::Received,
        ),
        message(
            "g5",
            me,
            t + 50 * MIN,
            "Отлично, тогда вечером выпускаем 🎉",
            MessageState::Read,
        ),
        message(
            "g6",
            me,
            t + 51 * MIN,
            "Отправляется…",
            MessageState::Pending,
        ),
    ];
    list[3].reply = Some(ReplyView {
        message_id: "g3".into(),
        author_name: "Глеб".into(),
        text: "Проверю звонки на ноутбуке после обеда.".into(),
        deleted: false,
    });
    list[5].server_sequence = 0;
    list
}

fn rows(list: &[MessageView]) -> ModelRc<MessageRow> {
    let rows: Vec<MessageRow> = chat::message_rows(list, 0, NOW_MS)
        .into_iter()
        .map(|r| MessageRow {
            key: r.key.into(),
            author: r.author.into(),
            time: r.time.into(),
            body: r.body.into(),
            mine: r.mine,
            grouped: r.grouped,
            has_reply: r.has_reply,
            reply_author: r.reply_author.into(),
            reply_text: r.reply_text.into(),
            edited: r.edited,
            deleted: r.deleted,
            state: r.state.into(),
            failed: r.failed,
            error: chat::error_text(&r.error).into(),
            initials: r.initials.into(),
            avatar: r.avatar,
            clock: r.clock.into(),
            day: r.day.into(),
        })
        .collect();
    ModelRc::new(VecModel::from(rows))
}

#[allow(clippy::too_many_arguments)]
fn conversation(
    id: &str,
    title: &str,
    peer: &str,
    online: bool,
    unread: i32,
    last: &str,
    ago_ms: i64,
    group: bool,
) -> ConversationRow {
    ConversationRow {
        id: id.into(),
        title: title.into(),
        username: if group {
            "".into()
        } else {
            peer.trim_start_matches("u-").into()
        },
        peer_id: if group { "".into() } else { peer.into() },
        online,
        unread,
        last_text: last.into(),
        can_send: true,
        blocked_by_me: false,
        has_draft: false,
        group,
        initials: chat::initials(title).into(),
        avatar: chat::avatar_index(if group { id } else { peer }),
        last_time: chat::short_when(NOW_MS - ago_ms, 0, NOW_MS).into(),
    }
}

fn conversations() -> ModelRc<ConversationRow> {
    let mut list = vec![
        conversation(
            "c-bob",
            "Борис",
            "u-bob",
            true,
            0,
            "Созвонимся вечером?",
            9 * MIN,
            false,
        ),
        conversation(
            "c-team",
            "Команда проекта",
            "",
            false,
            4,
            "Вера: Сборка 0.1.5 готова",
            2 * HOUR,
            true,
        ),
        conversation(
            "c-vera",
            "Вера Смирнова",
            "u-vera",
            true,
            2,
            "Посмотри, пожалуйста, макет",
            3 * HOUR,
            false,
        ),
        conversation(
            "c-gleb",
            "Глеб",
            "u-gleb",
            false,
            0,
            "Хорошо, до завтра!",
            DAY,
            false,
        ),
        conversation("c-dina", "Дина", "u-dina", false, 0, "", 4 * DAY, false),
        conversation(
            "c-family",
            "Семейный чат",
            "",
            false,
            128,
            "Кто заберёт посылку?",
            6 * DAY,
            true,
        ),
        conversation(
            "c-egor",
            "Егор",
            "u-egor",
            true,
            0,
            "Спасибо!",
            40 * DAY,
            false,
        ),
    ];
    list[4].has_draft = true;
    ModelRc::new(VecModel::from(list))
}

fn members() -> ModelRc<MemberRow> {
    let m = |id: &str, name: &str, speaking: bool, volume: f32, info: &str| MemberRow {
        peer_id: id.into(),
        name: name.into(),
        speaking,
        volume,
        info: info.into(),
        initials: chat::initials(name).into(),
        avatar: chat::avatar_index(id),
    };
    ModelRc::new(VecModel::from(vec![
        m("u-bob", "Борис", true, 100.0, "звук · 38 мс"),
        m("u-vera", "Вера Смирнова", false, 140.0, "звук · 52 мс"),
        m("u-gleb", "Глеб", false, 80.0, "нет звука"),
    ]))
}

/// A made-up "desktop" picture for the watched share.
fn fake_screen(w: u32, h: u32) -> Image {
    let mut buf = SharedPixelBuffer::<Rgb8Pixel>::new(w, h);
    let width = w as usize;
    for (i, p) in buf.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = ((i % width) as u32, (i / width) as u32);
        let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
        let mut c = Rgb8Pixel {
            r: (30.0 + 60.0 * fx) as u8,
            g: (40.0 + 50.0 * fy) as u8,
            b: (90.0 + 80.0 * (1.0 - fx)) as u8,
        };
        // Two "windows" and a task bar.
        let in_win = |x0: f32, y0: f32, x1: f32, y1: f32| fx > x0 && fx < x1 && fy > y0 && fy < y1;
        if in_win(0.08, 0.1, 0.55, 0.7) {
            c = if fy < 0.15 {
                Rgb8Pixel {
                    r: 60,
                    g: 63,
                    b: 72,
                }
            } else {
                Rgb8Pixel {
                    r: 236,
                    g: 238,
                    b: 242,
                }
            };
            if fy > 0.2 && (y / 12) % 2 == 0 && fx < 0.45 {
                c = Rgb8Pixel {
                    r: 170,
                    g: 176,
                    b: 190,
                };
            }
        } else if in_win(0.5, 0.3, 0.92, 0.85) {
            c = if fy < 0.35 {
                Rgb8Pixel {
                    r: 50,
                    g: 52,
                    b: 60,
                }
            } else {
                Rgb8Pixel {
                    r: 37,
                    g: 38,
                    b: 44,
                }
            };
        } else if fy > 0.94 {
            c = Rgb8Pixel {
                r: 20,
                g: 21,
                b: 25,
            };
        }
        *p = c;
    }
    Image::from_rgb8(buf)
}

fn fill(ui: &MainWindow, scene: Scene) {
    ui.set_build_info("turbo-pancake 0.1.4 · снимок интерфейса".into());
    ui.set_server_label("messenger.example:7878".into());
    ui.set_inputs(ModelRc::new(VecModel::from(vec![
        "По умолчанию".into(),
        "Микрофон (USB Audio)".into(),
    ])));
    ui.set_outputs(ModelRc::new(VecModel::from(vec![
        "По умолчанию".into(),
        "Наушники".into(),
    ])));
    ui.set_dsp_available(true);
    ui.set_mic_level(0.62);
    ui.set_monitors(ModelRc::new(VecModel::from(vec![
        "Монитор 1 · 2560×1440".into(),
        "Монитор 2 · 1920×1080".into(),
    ])));
    if scene == Scene::SignIn {
        ui.set_connection_status("Не в сети".into());
        return;
    }
    ui.set_signed_in(true);
    ui.set_user_name("Алиса".into());
    ui.set_user_initials(chat::initials("Алиса").into());
    ui.set_user_avatar(chat::avatar_index("u-me"));
    ui.set_connection_status("В сети".into());
    ui.set_conversations(conversations());
    ui.set_unread_total(134);
    ui.set_update_available(true);
    ui.set_update_text("Доступна версия 0.1.5: проверена и готова к установке".into());
    ui.set_update_button("Установить".into());
    ui.set_tray_available(true);
    match scene {
        Scene::Chat | Scene::Incoming => {
            ui.set_current_conversation("c-bob".into());
            ui.set_chat_title("Борис".into());
            ui.set_chat_username("bob".into());
            ui.set_chat_initials("Б".into());
            ui.set_chat_avatar(chat::avatar_index("u-bob"));
            ui.set_chat_online(true);
            ui.set_messages(rows(&direct_messages()));
            ui.set_compose_mode("reply".into());
            ui.set_compose_label("Ответ Борису: Созвонимся вечером, обсудим планы…".into());
            ui.set_compose_text("Конечно, в семь удобно".into());
            if scene == Scene::Incoming {
                ui.set_incoming_title("Команда проекта".into());
            }
        }
        Scene::Group => {
            ui.set_current_conversation("c-team".into());
            ui.set_chat_title("Команда проекта".into());
            ui.set_chat_group(true);
            ui.set_chat_can_manage(true);
            ui.set_chat_initials("КП".into());
            ui.set_chat_avatar(chat::avatar_index("c-team"));
            ui.set_chat_call_label("Войти в звонок (3)".into());
            ui.set_active_call_title("Команда проекта".into());
            ui.set_has_participation(true);
            ui.set_in_room(true);
            ui.set_muted(true);
            ui.set_group_settings_open(true);
            ui.set_banner_kind(2);
            ui.set_banner_text("Нет связи с сервером, повтор через 3 с".into());
            let gm = |id: &str, name: &str, user: &str, creator: bool| GroupMemberRow {
                id: id.into(),
                name: format!("{name} · @{user}").into(),
                creator,
                initials: chat::initials(name).into(),
                avatar: chat::avatar_index(id),
            };
            ui.set_group_members(ModelRc::new(VecModel::from(vec![
                gm("u-vera", "Вера Смирнова", "vera", true),
                gm("u-me", "Алиса", "alice", false),
                gm("u-bob", "Борис", "bob", false),
                gm("u-gleb", "Глеб", "gleb", false),
            ])));
            ui.set_messages(rows(&group_messages()));
        }
        Scene::Call
        | Scene::CallWaiting
        | Scene::CallMuted
        | Scene::CallShares
        | Scene::CallSettings => {
            ui.set_voice_selected(true);
            ui.set_in_room(true);
            ui.set_has_participation(true);
            ui.set_call_stats("RTT 38 мс · потери 0,1 % · джиттер 4 мс · 64 кбит/с".into());
            ui.set_call_devices("Микрофон: USB Audio · вывод: наушники · 48 кГц".into());
            if scene == Scene::CallWaiting {
                ui.set_active_call_title("Борис".into());
                ui.set_call_phase(2);
            } else {
                ui.set_active_call_title("Команда проекта".into());
                ui.set_call_phase(3);
                ui.set_members(members());
            }
            ui.set_muted(scene == Scene::CallMuted);
            ui.set_voice_activation(true);
            ui.set_self_speaking(scene == Scene::CallSettings);
            if scene == Scene::CallSettings {
                ui.set_voice_panel_open(true);
            }
            if scene == Scene::CallShares {
                let shares = vec![
                    ShareRow {
                        peer_id: "u-vera".into(),
                        name: "Вера Смирнова".into(),
                        info: "1080p · 60 к/с".into(),
                        watching: true,
                        preview: Image::default(),
                        has_preview: false,
                        preview_ssrc: 0,
                    },
                    ShareRow {
                        peer_id: "u-gleb".into(),
                        name: "Глеб".into(),
                        info: "1440p · 30 к/с".into(),
                        watching: false,
                        preview: fake_screen(320, 180),
                        has_preview: true,
                        preview_ssrc: 7,
                    },
                ];
                ui.set_shares(ModelRc::new(VecModel::from(shares)));
                ui.set_watching("u-vera".into());
                ui.set_watching_name("Вера Смирнова".into());
                ui.set_screen_picture(fake_screen(1280, 720));
                ui.set_has_picture(true);
                ui.set_layer_info("высокое качество · 1920×1080 · 60 к/с".into());
                ui.set_viewer_info("задержка 70 мс".into());
                ui.set_sharing(true);
            }
        }
        Scene::SettingsLook | Scene::SettingsAlerts | Scene::SettingsVoice => {
            ui.set_current_conversation("c-bob".into());
            ui.set_chat_title("Борис".into());
            ui.set_chat_username("bob".into());
            ui.set_chat_initials("Б".into());
            ui.set_chat_avatar(chat::avatar_index("u-bob"));
            ui.set_messages(rows(&direct_messages()));
            ui.set_app_settings_open(true);
            ui.set_settings_section(match scene {
                Scene::SettingsLook => 0,
                Scene::SettingsAlerts => 1,
                _ => 2,
            });
        }
        Scene::SignIn | Scene::Empty => {}
    }
}

fn render(
    out: &Path,
    file: &str,
    scene: Scene,
    dark: bool,
    (w, h): (u32, u32),
    scale: f32,
    pointer: Option<(f32, f32)>,
) -> Result<(), Box<dyn std::error::Error>> {
    let ui = MainWindow::new()?;
    let window = WINDOW
        .with_borrow(|w| w.clone())
        .ok_or("no window adapter")?;
    window.dispatch_event(WindowEvent::ScaleFactorChanged {
        scale_factor: scale,
    });
    let (pw, ph) = ((w as f32 * scale) as u32, (h as f32 * scale) as u32);
    window.set_size(PhysicalSize::new(pw, ph));
    ui.invoke_apply_theme(dark);
    fill(&ui, scene);
    ui.show()?;
    advance(600);
    if let Some((x, y)) = pointer {
        window.dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(x, y),
        });
        advance(600);
    }
    save(&window, &out.join(file), pw, ph)?;
    ui.hide()?;
    Ok(())
}

/// Draws the window into a buffer and writes it as an RGB PNG.
fn save(
    window: &MinimalSoftwareWindow,
    path: &Path,
    pw: u32,
    ph: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut buffer = vec![Rgb8Pixel { r: 0, g: 0, b: 0 }; (pw * ph) as usize];
    window.request_redraw();
    window.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, pw as usize);
    });
    let writer = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut encoder = png::Encoder::new(writer, pw, ph);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let bytes: Vec<u8> = buffer.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
    encoder.write_header()?.write_image_data(&bytes)?;
    println!("{}", path.display());
    Ok(())
}

/// The separate share window with its overlay bar (pointer over the window).
fn render_share_window(out: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let ui = ShareWindow::new()?;
    let window = WINDOW
        .with_borrow(|w| w.clone())
        .ok_or("no window adapter")?;
    let (pw, ph) = (1280, 720);
    window.set_size(PhysicalSize::new(pw, ph));
    ui.set_share_name("Вера Смирнова".into());
    ui.set_info("1080p · 60 к/с · задержка 70 мс".into());
    ui.set_picture(fake_screen(1280, 720));
    ui.set_has_picture(true);
    ui.show()?;
    advance(300);
    window.dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(640.0, 400.0),
    });
    advance(600);
    save(&window, &out.join("share-window.png"), pw, ph)?;
    ui.hide()?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out: PathBuf = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".local/logs/ui-snapshots"));
    std::fs::create_dir_all(&out)?;
    slint::platform::set_platform(Box::new(Offscreen)).map_err(|e| format!("{e:?}"))?;
    let full = (1280, 760);
    render_share_window(&out)?;
    let scenes = [
        (Scene::SignIn, None),
        (Scene::Empty, None),
        // The pointer over a message shows the hover toolbar and grouped-line times.
        (Scene::Chat, Some((900.0, 420.0))),
        (Scene::Group, None),
        (Scene::Call, None),
        (Scene::CallWaiting, None),
        (Scene::CallMuted, None),
        (Scene::CallShares, None),
        (Scene::CallSettings, None),
        (Scene::Incoming, None),
        (Scene::SettingsLook, None),
        (Scene::SettingsAlerts, None),
        (Scene::SettingsVoice, None),
    ];
    for (scene, pointer) in scenes {
        for dark in [true, false] {
            let theme = if dark { "dark" } else { "light" };
            render(
                &out,
                &format!("{}-{theme}.png", scene.name()),
                scene,
                dark,
                full,
                1.0,
                pointer,
            )?;
        }
    }
    for dark in [true, false] {
        let theme = if dark { "dark" } else { "light" };
        render(
            &out,
            &format!("compact-chat-{theme}.png"),
            Scene::Chat,
            dark,
            (760, 760),
            1.0,
            None,
        )?;
        render(
            &out,
            &format!("compact-call-{theme}.png"),
            Scene::Call,
            dark,
            (760, 760),
            1.0,
            None,
        )?;
    }
    render(
        &out,
        "scale150-chat-dark.png",
        Scene::Chat,
        true,
        full,
        1.5,
        None,
    )?;
    render(
        &out,
        "scale150-call-dark.png",
        Scene::Call,
        true,
        full,
        1.5,
        None,
    )?;
    Ok(())
}
