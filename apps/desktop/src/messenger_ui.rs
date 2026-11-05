//! Wires the messenger service (crates/messenger-client) to the main window: commands
//! from Slint callbacks, views back into models through the event loop. The service
//! thread never touches the UI and the UI never waits on the network.

use std::cell::RefCell;
use std::rc::Rc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use turbo_pancake_desktop::chat::{self, Compose};
use turbo_pancake_desktop::credentials;
use turbo_pancake_messenger_client::{
    Command, ConversationView, MessageView, Service, ServiceConfig, Status, Update,
};

use crate::{ConversationRow, MainWindow, MessageRow};

/// Scripted use for trials without input (`--login`, `--chat-with`, `--send`).
#[derive(Clone, Debug, Default)]
pub struct Script {
    pub profile: String,
    pub login: Option<String>,
    pub register: bool,
    pub chat_with: Option<String>,
    pub send: Vec<String>,
    /// Block the `chat_with` user once the conversation is open.
    pub block: bool,
    pub chat_log: bool,
}

#[derive(Default)]
struct UiState {
    current: Option<String>,
    compose: Compose,
    conversations: Vec<ConversationView>,
    messages: Vec<MessageView>,
    signed_in: bool,
    /// `--send` texts waiting for the first opened conversation.
    queued_sends: Vec<String>,
    queued_block: bool,
    chat_log: bool,
}

thread_local! {
    static STATE: RefCell<UiState> = RefCell::new(UiState::default());
}

pub struct Messenger {
    service: Rc<Service>,
}

fn local_offset_s() -> i32 {
    time::UtcOffset::current_local_offset()
        .map(|o| o.whole_seconds())
        .unwrap_or(0)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn cache_dir(profile: &str) -> Option<std::path::PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("HOME"))?;
    Some(
        std::path::PathBuf::from(base)
            .join("turbo-pancake")
            .join("cache")
            .join(profile),
    )
}

pub fn start(
    window: &MainWindow,
    server: &str,
    script: Script,
    tls: turbo_pancake_messenger_client::api::TlsConfig,
) -> Messenger {
    let http = chat::http_base(server);
    window.set_server_label(chat::server_label(&http).into());
    let target = credentials::target(&http, &script.profile);
    STATE.with_borrow_mut(|s| {
        s.queued_sends = script.send.clone();
        s.queued_block = script.block;
        s.chat_log = script.chat_log;
    });

    let weak = window.as_weak();
    let cred_target = target.clone();
    let service = Rc::new(Service::start(
        ServiceConfig {
            server: http,
            device_name: device_name(),
            client_version: format!(
                "turbo-pancake-desktop/{} ({})",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::OS
            ),
            cache_dir: cache_dir(&script.profile),
            view_limit: 300,
            tls: Some(tls),
        },
        move |u| {
            // Secrets are stored here, on the service thread, never passed to the UI.
            match &u {
                Update::SignedIn { user, token } => {
                    if let Err(e) = credentials::save(&cred_target, token, user) {
                        eprintln!("could not store the session: {e}");
                    }
                }
                Update::SignedOut { .. } => credentials::delete(&cred_target),
                _ => {}
            }
            let u = match u {
                Update::SignedIn { user, .. } => Update::SignedIn {
                    user,
                    token: String::new(),
                },
                other => other,
            };
            let _ = weak.upgrade_in_event_loop(move |w| apply(&w, u));
        },
    ));

    wire(window, &service);

    // Start: scripted sign-in, else the stored session (works offline from the cache).
    if let Some(name) = &script.login {
        let password = std::env::var("TURBO_PANCAKE_PASSWORD").unwrap_or_default();
        window.set_auth_busy(true);
        service.send(if script.register {
            Command::Register {
                username: name.clone(),
                password,
                display_name: String::new(),
            }
        } else {
            Command::Login {
                username: name.clone(),
                password,
            }
        });
    } else if let Some((token, user)) = credentials::load(&target) {
        window.set_user_name(user.display_name.as_str().into());
        service.send(Command::Resume { token, user });
    }
    if let Some(peer) = script.chat_with {
        service.send(Command::StartDirect { username: peer });
    }
    Messenger { service }
}

impl Messenger {
    pub fn stop(self) {
        drop(self.service);
    }
}

fn device_name() -> String {
    let host = std::env::var("COMPUTERNAME").unwrap_or_default();
    let os = std::env::consts::OS;
    let s = if host.is_empty() {
        format!("desktop ({os})")
    } else {
        format!("{host} ({os})")
    };
    s.chars()
        .take(turbo_pancake_protocol::MAX_DEVICE_NAME_CHARS)
        .collect()
}

fn wire(window: &MainWindow, service: &Rc<Service>) {
    let s = service.clone();
    let weak = window.as_weak();
    window.on_login(move |u, p| {
        if let Some(w) = weak.upgrade() {
            w.set_auth_busy(true);
            w.set_auth_error("".into());
        }
        s.send(Command::Login {
            username: u.trim().to_string(),
            password: p.to_string(),
        });
    });
    let s = service.clone();
    let weak = window.as_weak();
    window.on_register(move |u, p, d| {
        if let Some(w) = weak.upgrade() {
            w.set_auth_busy(true);
            w.set_auth_error("".into());
        }
        s.send(Command::Register {
            username: u.trim().to_string(),
            password: p.to_string(),
            display_name: d.trim().to_string(),
        });
    });
    let s = service.clone();
    window.on_logout(move || s.send(Command::Logout));
    let s = service.clone();
    let weak = window.as_weak();
    window.on_find_user(move |name| {
        if let Some(w) = weak.upgrade() {
            w.set_find_error("".into());
        }
        let name = name.trim().trim_start_matches('@').to_string();
        if !name.is_empty() {
            s.send(Command::StartDirect { username: name });
        }
    });
    let s = service.clone();
    let weak = window.as_weak();
    window.on_open_conversation(move |id| {
        if let Some(w) = weak.upgrade() {
            open(&w, &s, id.as_str());
        }
    });
    let s = service.clone();
    let weak = window.as_weak();
    window.on_open_voice(move || {
        if let Some(w) = weak.upgrade() {
            w.set_voice_selected(true);
        }
        s.send(Command::Open {
            conversation_id: None,
        });
    });
    let s = service.clone();
    let weak = window.as_weak();
    window.on_send_message(move |text| {
        let Some(w) = weak.upgrade() else { return };
        send(&w, &s, text.as_str());
    });
    let weak = window.as_weak();
    window.on_reply_to(move |key| {
        let Some(w) = weak.upgrade() else { return };
        let Some(m) = find_message(key.as_str()) else {
            return;
        };
        let snippet: String = m
            .body
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(60)
            .collect();
        set_compose(
            &w,
            Compose::Reply {
                message_id: m.message_id.clone(),
                label: format!("Ответ для {}: {snippet}", m.author_name),
            },
        );
    });
    let weak = window.as_weak();
    window.on_edit_message(move |key| {
        let Some(w) = weak.upgrade() else { return };
        let Some(m) = find_message(key.as_str()) else {
            return;
        };
        set_compose(
            &w,
            Compose::Edit {
                message_id: m.message_id.clone(),
            },
        );
        w.set_compose_text(m.body.as_str().into());
    });
    let s = service.clone();
    window.on_delete_message(move |key| {
        let (Some(m), Some(conv)) = (find_message(key.as_str()), current()) else {
            return;
        };
        if !m.message_id.is_empty() {
            s.send(Command::Delete {
                conversation_id: conv,
                message_id: m.message_id,
            });
        }
    });
    let s = service.clone();
    window.on_retry_message(move |key| {
        if let Some(m) = find_message(key.as_str())
            && !m.pending_nonce.is_empty()
        {
            s.send(Command::Retry {
                nonce: m.pending_nonce,
            });
        }
    });
    let s = service.clone();
    window.on_discard_message(move |key| {
        if let Some(m) = find_message(key.as_str())
            && !m.pending_nonce.is_empty()
        {
            s.send(Command::Discard {
                nonce: m.pending_nonce,
            });
        }
    });
    let weak = window.as_weak();
    window.on_cancel_compose(move || {
        let Some(w) = weak.upgrade() else { return };
        if matches!(
            STATE.with_borrow(|s| s.compose.clone()),
            Compose::Edit { .. }
        ) {
            w.set_compose_text("".into());
        }
        set_compose(&w, Compose::New);
    });
    let s = service.clone();
    window.on_toggle_block(move || {
        let Some(conv) = current() else { return };
        let row = STATE.with_borrow(|st| st.conversations.iter().find(|c| c.id == conv).cloned());
        if let Some(c) = row
            && !c.peer_user_id.is_empty()
        {
            s.send(Command::Block {
                user_id: c.peer_user_id,
                blocked: !c.blocked_by_me,
            });
        }
    });
    let s = service.clone();
    window.on_draft_changed(move |text| {
        let Some(conv) = current() else { return };
        let compose = STATE.with_borrow(|st| st.compose.clone());
        if !matches!(compose, Compose::Edit { .. }) {
            s.send(Command::SaveDraft {
                conversation_id: conv,
                body: text.to_string(),
                reply_to_id: compose.reply_to().to_owned(),
            });
        }
    });
}

fn current() -> Option<String> {
    STATE.with_borrow(|s| s.current.clone())
}

fn find_message(key: &str) -> Option<MessageView> {
    STATE.with_borrow(|s| s.messages.iter().find(|m| m.key == key).cloned())
}

fn set_compose(w: &MainWindow, c: Compose) {
    w.set_compose_mode(c.mode().into());
    w.set_compose_label(c.label().into());
    STATE.with_borrow_mut(|s| s.compose = c);
}

fn open(w: &MainWindow, service: &Service, id: &str) {
    let draft = STATE.with_borrow_mut(|s| {
        s.current = Some(id.to_owned());
        s.messages.clear();
        s.compose = Compose::New;
        s.conversations.iter().find(|c| c.id == id).cloned()
    });
    w.set_voice_selected(false);
    w.set_current_conversation(id.into());
    w.set_messages(ModelRc::new(VecModel::<MessageRow>::default()));
    w.set_compose_mode("".into());
    w.set_compose_label("".into());
    w.set_compose_text(
        draft
            .as_ref()
            .map(|c| c.draft.as_str())
            .unwrap_or("")
            .into(),
    );
    show_header(w);
    service.send(Command::Open {
        conversation_id: Some(id.to_owned()),
    });
}

fn send(w: &MainWindow, service: &Service, text: &str) {
    let Some(conv) = current() else { return };
    let text = text.trim_end();
    if text.trim().is_empty() {
        return;
    }
    let compose = STATE.with_borrow(|s| s.compose.clone());
    match compose {
        Compose::Edit { message_id } => service.send(Command::Edit {
            conversation_id: conv,
            message_id,
            body: text.to_owned(),
        }),
        other => service.send(Command::Send {
            conversation_id: conv,
            body: text.to_owned(),
            reply_to_id: other.reply_to().to_owned(),
        }),
    }
    w.set_compose_text("".into());
    set_compose(w, Compose::New);
}

fn show_header(w: &MainWindow) {
    let row = STATE.with_borrow(|s| {
        let cur = s.current.clone()?;
        s.conversations.iter().find(|c| c.id == cur).cloned()
    });
    if let Some(c) = row {
        w.set_chat_title(c.title.as_str().into());
        w.set_chat_username(c.peer_username.as_str().into());
        w.set_chat_online(c.online);
        w.set_chat_can_send(c.can_send);
        w.set_chat_blocked_by_me(c.blocked_by_me);
    }
}

fn status_text(s: &Status) -> String {
    match s {
        Status::SignedOut => "Не в сети".into(),
        Status::Connecting => "Подключение…".into(),
        Status::Online => "В сети".into(),
        Status::Reconnecting { retry_in_ms } => {
            format!("Нет связи, повтор через {} с", retry_in_ms.div_ceil(1000))
        }
    }
}

/// One update from the service, on the UI thread.
fn apply(w: &MainWindow, u: Update) {
    let log = STATE.with_borrow(|s| s.chat_log);
    match u {
        Update::Calls(_) | Update::CallEvent(_) | Update::CallReady { .. } => {}
        Update::Status(s) => {
            if STATE.with_borrow(|st| st.signed_in) {
                w.set_connection_status(status_text(&s).into());
            }
            if log {
                println!(
                    "TURBO_PANCAKE_CHAT status={}",
                    match s {
                        Status::SignedOut => "signed_out",
                        Status::Connecting => "connecting",
                        Status::Online => "online",
                        Status::Reconnecting { .. } => "reconnecting",
                    }
                );
            }
        }
        Update::SignedIn { user, .. } => {
            STATE.with_borrow_mut(|s| s.signed_in = true);
            w.set_signed_in(true);
            w.set_auth_busy(false);
            w.set_auth_error("".into());
            w.set_user_name(user.display_name.as_str().into());
            if log {
                println!("TURBO_PANCAKE_CHAT signed_in user={}", user.username);
            }
        }
        Update::SignedOut { reason } => {
            STATE.with_borrow_mut(|s| {
                let (log, queued) = (s.chat_log, std::mem::take(&mut s.queued_sends));
                *s = UiState::default();
                s.chat_log = log;
                s.queued_sends = queued;
            });
            w.set_signed_in(false);
            w.set_auth_busy(false);
            if reason != "signed out" {
                w.set_auth_error(chat::error_text(&reason).into());
            }
            w.set_current_conversation("".into());
            w.set_conversations(ModelRc::new(VecModel::<ConversationRow>::default()));
            w.set_messages(ModelRc::new(VecModel::<MessageRow>::default()));
            w.set_connection_status("Не в сети".into());
            if log {
                println!("TURBO_PANCAKE_CHAT signed_out reason=\"{reason}\"");
            }
        }
        Update::Conversations(list) => {
            let rows = chat::conversation_rows(&list);
            STATE.with_borrow_mut(|s| s.conversations = list);
            let model: Vec<ConversationRow> = rows
                .into_iter()
                .map(|r| ConversationRow {
                    id: r.id.into(),
                    title: r.title.into(),
                    username: r.username.into(),
                    peer_id: r.peer_id.into(),
                    online: r.online,
                    unread: r.unread,
                    last_text: r.last_text.into(),
                    can_send: r.can_send,
                    blocked_by_me: r.blocked_by_me,
                    has_draft: r.has_draft,
                })
                .collect();
            if log {
                let unread: i32 = model.iter().map(|r| r.unread).sum();
                println!(
                    "TURBO_PANCAKE_CHAT conversations={} unread={unread}",
                    model.len()
                );
            }
            update_model(w.get_conversations(), model, |m| w.set_conversations(m));
            show_header(w);
        }
        Update::Messages {
            conversation_id,
            messages,
        } => {
            if current().as_deref() != Some(conversation_id.as_str()) {
                return;
            }
            let rows = chat::message_rows(&messages, local_offset_s(), now_ms());
            STATE.with_borrow_mut(|s| s.messages = messages);
            if log {
                let last = rows.last();
                println!(
                    "TURBO_PANCAKE_CHAT messages={} last_body=\"{}\" last_state=\"{}\" failed={}",
                    rows.len(),
                    last.map_or("", |r| r.body.as_str()).replace('"', "'"),
                    last.map_or("", |r| r.state.as_str()),
                    rows.iter().filter(|r| r.failed).count(),
                );
            }
            let model: Vec<MessageRow> = rows
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
                })
                .collect();
            w.set_messages(ModelRc::new(VecModel::from(model)));
        }
        Update::Opened { conversation_id } => {
            open_from_service(w, &conversation_id);
        }
        Update::Closed { conversation_id } => {
            if current().as_deref() == Some(conversation_id.as_str()) {
                STATE.with_borrow_mut(|s| {
                    s.current = None;
                    s.messages.clear();
                });
                set_compose(w, Compose::New);
                w.set_current_conversation("".into());
                w.set_compose_text("".into());
                w.set_messages(ModelRc::new(VecModel::<MessageRow>::default()));
                w.set_chat_can_send(false);
            }
        }
        Update::Error(e) => {
            let text: SharedString = chat::error_text(&e).into();
            if STATE.with_borrow(|s| s.signed_in) {
                w.set_find_error(text);
            } else {
                w.set_auth_busy(false);
                w.set_auth_error(text);
            }
            if log {
                println!("TURBO_PANCAKE_CHAT error=\"{}\"", e.replace('"', "'"));
            }
        }
    }
}

/// `StartDirect` found the user: select the conversation (the service already opened it)
/// and send any scripted messages.
fn open_from_service(w: &MainWindow, id: &str) {
    STATE.with_borrow_mut(|s| {
        s.current = Some(id.to_owned());
        s.compose = Compose::New;
    });
    w.set_voice_selected(false);
    w.set_current_conversation(id.into());
    w.set_compose_mode("".into());
    show_header(w);
    let queued = STATE.with_borrow_mut(|s| std::mem::take(&mut s.queued_sends));
    if STATE.with_borrow_mut(|s| std::mem::take(&mut s.queued_block)) {
        let weak = w.as_weak();
        // After the conversation list (with the peer's id) has arrived.
        slint::Timer::single_shot(std::time::Duration::from_millis(800), move || {
            if let Some(w) = weak.upgrade() {
                w.invoke_toggle_block();
            }
        });
    }
    if !queued.is_empty() {
        let weak = w.as_weak();
        // Runs after the current callbacks: send through the normal input path.
        slint::Timer::single_shot(std::time::Duration::from_millis(10), move || {
            if let Some(w) = weak.upgrade() {
                for t in queued {
                    w.invoke_send_message(t.into());
                }
            }
        });
    }
}

/// Replaces rows in place when the length is unchanged (keeps the list's scroll position).
fn update_model<T: Clone + PartialEq + 'static>(
    current: ModelRc<T>,
    rows: Vec<T>,
    set: impl FnOnce(ModelRc<T>),
) {
    if let Some(vm) = current.as_any().downcast_ref::<VecModel<T>>()
        && vm.row_count() == rows.len()
    {
        for (i, r) in rows.into_iter().enumerate() {
            if vm.row_data(i).as_ref() != Some(&r) {
                vm.set_row_data(i, r);
            }
        }
        return;
    }
    set(ModelRc::new(VecModel::from(rows)));
}
