//! Messenger view helpers (UI-free, tested headless): rows for the Slint lists, times in
//! local time, the HTTP base of the server, the compose state of the input line.

use turbo_pancake_messenger_client::{ConversationView, MessageState, MessageView};

/// `ws://host:port/v1/signal` (the `--server` form) → `http://host:port`.
pub fn http_base(server: &str) -> String {
    let s = server.trim().trim_end_matches('/');
    let s = s.strip_suffix("/v1/signal").unwrap_or(s);
    if let Some(rest) = s.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = s.strip_prefix("ws://") {
        format!("http://{rest}")
    } else if s.starts_with("http://") || s.starts_with("https://") {
        s.to_owned()
    } else {
        format!("http://{s}")
    }
}

/// Host and port only (shown on the sign-in screen).
pub fn server_label(http: &str) -> String {
    http.split("://").nth(1).unwrap_or(http).to_owned()
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConversationRowData {
    pub id: String,
    pub title: String,
    pub username: String,
    pub peer_id: String,
    pub online: bool,
    pub unread: i32,
    pub last_text: String,
    pub can_send: bool,
    pub blocked_by_me: bool,
    pub has_draft: bool,
}

pub fn conversation_rows(list: &[ConversationView]) -> Vec<ConversationRowData> {
    list.iter()
        .map(|c| ConversationRowData {
            id: c.id.clone(),
            title: c.title.clone(),
            username: c.peer_username.clone(),
            peer_id: c.peer_user_id.clone(),
            online: c.online,
            unread: c.unread.min(i64::from(i32::MAX)) as i32,
            last_text: c.last_text.clone(),
            can_send: c.can_send,
            blocked_by_me: c.blocked_by_me,
            has_draft: !c.draft.is_empty(),
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct MessageRowData {
    pub key: String,
    pub author: String,
    pub time: String,
    pub body: String,
    pub mine: bool,
    pub grouped: bool,
    pub has_reply: bool,
    pub reply_author: String,
    pub reply_text: String,
    pub edited: bool,
    pub deleted: bool,
    pub state: String,
    pub failed: bool,
    pub error: String,
}

/// Messages from one author within this window are shown without a new header.
const GROUP_MS: i64 = 7 * 60 * 1000;

pub fn message_rows(list: &[MessageView], utc_offset_s: i32, now_ms: i64) -> Vec<MessageRowData> {
    let mut out: Vec<MessageRowData> = Vec::with_capacity(list.len());
    for (i, m) in list.iter().enumerate() {
        let prev = i.checked_sub(1).map(|p| &list[p]);
        let grouped = m.reply.is_none()
            && prev.is_some_and(|p| {
                p.author_id == m.author_id && (m.created_at_ms - p.created_at_ms).abs() < GROUP_MS
            });
        let (state, failed, error) = match &m.state {
            MessageState::Received => (String::new(), false, String::new()),
            MessageState::Pending => ("отправляется…".to_owned(), false, String::new()),
            MessageState::Failed(e) => (String::new(), true, e.clone()),
            MessageState::Sent => ("отправлено".to_owned(), false, String::new()),
            MessageState::Delivered => ("доставлено".to_owned(), false, String::new()),
            MessageState::Read => ("прочитано".to_owned(), false, String::new()),
        };
        // Only the newest own message carries its delivery state (like a read marker);
        // pending and failed ones always show theirs.
        let state = if matches!(
            m.state,
            MessageState::Sent | MessageState::Delivered | MessageState::Read
        ) && list[i + 1..]
            .iter()
            .any(|n| n.mine && n.server_sequence > 0)
        {
            String::new()
        } else {
            state
        };
        let (has_reply, reply_author, reply_text) = match &m.reply {
            Some(r) => (
                true,
                if r.author_name.is_empty() {
                    "…".to_owned()
                } else {
                    r.author_name.clone()
                },
                if r.deleted {
                    "сообщение удалено".to_owned()
                } else if r.text.is_empty() {
                    "сообщение недоступно".to_owned()
                } else {
                    r.text.clone()
                },
            ),
            None => (false, String::new(), String::new()),
        };
        out.push(MessageRowData {
            key: m.key.clone(),
            author: m.author_name.clone(),
            time: format_time(m.created_at_ms, utc_offset_s, now_ms),
            body: m.body.clone(),
            mine: m.mine,
            grouped,
            has_reply,
            reply_author,
            reply_text,
            edited: m.edited,
            deleted: m.deleted,
            state: if m.state == MessageState::Pending {
                "отправляется".to_owned()
            } else {
                state
            },
            failed,
            error,
        });
    }
    out
}

/// "сегодня в 14:05", "вчера в 09:30" or "03.11.2026 22:17" in local time.
pub fn format_time(ms: i64, utc_offset_s: i32, now_ms: i64) -> String {
    let local = |t: i64| t.div_euclid(1000) + i64::from(utc_offset_s);
    let (secs, now) = (local(ms), local(now_ms));
    let (day, today) = (secs.div_euclid(86_400), now.div_euclid(86_400));
    let tod = secs.rem_euclid(86_400);
    let hm = format!("{:02}:{:02}", tod / 3600, (tod % 3600) / 60);
    match today - day {
        0 => format!("сегодня в {hm}"),
        1 => format!("вчера в {hm}"),
        _ => {
            let (y, m, d) = civil_from_days(day);
            format!("{d:02}.{m:02}.{y} {hm}")
        }
    }
}

/// Days since 1970-01-01 → (year, month, day) (H. Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// What the input line does on Enter.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Compose {
    #[default]
    New,
    Reply {
        message_id: String,
        label: String,
    },
    Edit {
        message_id: String,
    },
}

impl Compose {
    pub fn mode(&self) -> &'static str {
        match self {
            Compose::New => "",
            Compose::Reply { .. } => "reply",
            Compose::Edit { .. } => "edit",
        }
    }

    pub fn label(&self) -> String {
        match self {
            Compose::New => String::new(),
            Compose::Reply { label, .. } => label.clone(),
            Compose::Edit { .. } => "Изменение сообщения (Enter — сохранить)".to_owned(),
        }
    }

    pub fn reply_to(&self) -> &str {
        match self {
            Compose::Reply { message_id, .. } => message_id,
            _ => "",
        }
    }
}

/// Russian text for client errors coming from the server or the network.
pub fn error_text(e: &str) -> String {
    match e {
        "wrong username or password" => "Неверное имя пользователя или пароль".into(),
        "username is taken" => "Это имя пользователя уже занято".into(),
        "not found" | "no such user" => "Пользователь не найден".into(),
        "you cannot message this user" => "Вы не можете писать этому пользователю".into(),
        "too many requests, try again later" => "Слишком много попыток, подождите".into(),
        "session ended, sign in again" => "Сессия завершена, войдите снова".into(),
        "this session was ended on another device" => {
            "Сессия завершена на другом устройстве".into()
        }
        "messenger is not enabled" => "На сервере не включены переписки".into(),
        e if e.contains("server certificate is not trusted") => {
            "Сертификат сервера не доверенный: проверьте файл --server-ca".into()
        }
        e if e.starts_with("server unreachable") => "Сервер недоступен".into(),
        e if e.starts_with("username must be") => {
            "Имя: 3–32 символа, латиница, цифры и _, начинается с буквы".into()
        }
        e if e.starts_with("password must be") => "Пароль: от 8 до 128 символов".into(),
        e if e.starts_with("message must be") => {
            "Сообщение пустое или длиннее 4000 символов".into()
        }
        e if e.starts_with("display name") => "Отображаемое имя: 1–64 символа".into(),
        e => e.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use turbo_pancake_messenger_client::ReplyView;

    #[test]
    fn server_urls() {
        assert_eq!(
            http_base("ws://127.0.0.1:7878/v1/signal"),
            "http://127.0.0.1:7878"
        );
        assert_eq!(
            http_base("wss://example.test/v1/signal/"),
            "https://example.test"
        );
        assert_eq!(http_base("127.0.0.1:7878"), "http://127.0.0.1:7878");
        assert_eq!(server_label("http://127.0.0.1:7878"), "127.0.0.1:7878");
    }

    #[test]
    fn local_times() {
        // 2026-11-04 12:00:00 UTC, Moscow (+3).
        let now = 1_793_793_600_000;
        assert_eq!(format_time(now, 3 * 3600, now), "сегодня в 15:00");
        assert_eq!(
            format_time(now - 86_400_000, 3 * 3600, now),
            "вчера в 15:00"
        );
        assert_eq!(
            format_time(now - 3 * 86_400_000, 0, now),
            "01.11.2026 12:00"
        );
    }

    fn msg(key: &str, author: &str, at: i64, state: MessageState) -> MessageView {
        MessageView {
            key: key.into(),
            message_id: key.into(),
            author_id: author.into(),
            author_name: author.to_uppercase(),
            mine: author == "me",
            body: key.into(),
            reply: None,
            created_at_ms: at,
            edited: false,
            deleted: false,
            state,
            server_sequence: 1,
            pending_nonce: String::new(),
        }
    }

    #[test]
    fn grouping_and_states() {
        let mut list = vec![
            msg("a", "bob", 0, MessageState::Received),
            msg("b", "bob", 60_000, MessageState::Received),
            msg("c", "me", 120_000, MessageState::Read),
            msg("d", "me", 180_000, MessageState::Delivered),
            msg(
                "e",
                "me",
                200_000,
                MessageState::Failed("you cannot message this user".into()),
            ),
        ];
        // Not accepted by the server: no sequence.
        list[4].server_sequence = 0;
        list[1].reply = Some(ReplyView {
            message_id: "a".into(),
            author_name: "BOB".into(),
            text: String::new(),
            deleted: true,
        });
        let rows = message_rows(&list, 0, 0);
        assert!(!rows[0].grouped);
        assert!(!rows[1].grouped, "a reply starts a new header");
        assert_eq!(rows[1].reply_text, "сообщение удалено");
        assert!(!rows[2].grouped);
        assert_eq!(rows[2].state, "", "older own message: no state line");
        assert_eq!(rows[3].state, "доставлено");
        assert!(rows[4].failed && rows[4].grouped);
        assert_eq!(
            error_text(&rows[4].error),
            "Вы не можете писать этому пользователю"
        );
    }
}
