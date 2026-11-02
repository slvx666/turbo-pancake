//! Text the UI shows for a [`Snapshot`] (formatted on the session thread, so the UI
//! thread only assigns strings) and the machine-readable `--call-log` line.

use turbo_pancake_media_transport::call::CallState;

use crate::session::{MediaTransport, Member, ServerStatus, Snapshot};

/// Lowest level on the microphone meter and the voice activation slider, dBFS.
pub const METER_FLOOR_DB: f32 = -60.0;

/// Position 0–1 of a dBFS level on the microphone meter.
pub fn meter_position(db: f32) -> f32 {
    ((db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0)
}

/// One row of the member list.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemberView {
    pub peer_id: String,
    pub name: String,
    pub speaking: bool,
    /// Their local volume in percent (0–200 on the slider).
    pub volume_pct: f32,
    /// Stream health in one short line ("нет звука" until their stream plays).
    pub info: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CallView {
    pub connection: String,
    pub in_room: bool,
    pub title: String,
    pub detail: String,
    pub stats: String,
    pub devices: String,
    pub error: String,
    /// Microphone level on the meter, 0–1 (dBFS scale from [`METER_FLOOR_DB`]; 0 muted).
    pub mic_level: f32,
    /// Own speaking indicator.
    pub speaking: bool,
    pub dsp_available: bool,
    pub members: Vec<MemberView>,
}

fn member_view(m: &Member) -> MemberView {
    let info = match &m.stream {
        None if m.ssrc.is_none() => "подключается…".to_string(),
        None => "нет звука".to_string(),
        Some(p) => {
            let j = &p.stats.jitter;
            format!(
                "буфер {} мс · маскировано {:.1} %",
                j.target_delay_ms,
                p.stats.concealed_ratio() * 100.0
            )
        }
    };
    MemberView {
        peer_id: m.peer_id.clone(),
        name: m.name.clone(),
        speaking: m.speaking(),
        volume_pct: m.volume * 100.0,
        info,
    }
}

pub fn render(s: &Snapshot) -> CallView {
    let connection = match &s.server {
        ServerStatus::Connecting => "Подключение к серверу…".to_string(),
        ServerStatus::Connected => match s.signaling_rtt_ms {
            Some(rtt) => format!("В сети · {rtt:.0} мс"),
            None => "В сети".to_string(),
        },
        ServerStatus::Disconnected(why) => format!("Нет связи: {why}"),
    };
    let names = s.member_names().join(", ");
    let (title, detail) = match &s.room {
        None => (
            "Не в звонке".to_string(),
            "Введите id комнаты и нажмите «Войти»; второй участник входит в ту же комнату."
                .to_string(),
        ),
        Some(room) if !s.joined => (format!("Комната «{room}»"), "Вход в комнату…".to_string()),
        Some(room) => {
            let state = match s.media.as_ref().map(|m| &m.state) {
                None | Some(CallState::Negotiating | CallState::Connecting) => {
                    "Подключение к медиасерверу…".to_string()
                }
                Some(CallState::Connected) if s.members.is_empty() => {
                    "Ожидание собеседника…".to_string()
                }
                Some(CallState::Connected) => format!("В звонке: {names}"),
                Some(CallState::Reconnecting) => "Связь потеряна, восстановление…".to_string(),
                Some(CallState::Closed) => "Звонок завершён".to_string(),
                Some(CallState::Failed(e)) => format!("Ошибка звонка: {e}"),
            };
            (format!("Комната «{room}»"), state)
        }
    };
    let mut stats = String::new();
    if let Some(c) = &s.call {
        let pct = |l: Option<f32>| l.map_or("—".to_string(), |l| format!("{:.1} %", l * 100.0));
        stats = format!(
            "RTT {} · потери ↑{} ↓{} · пакеты ↑{} ↓{}",
            c.rtt_ms.map_or("—".to_string(), |r| format!("{r:.0} мс")),
            pct(c.egress_loss),
            pct(c.ingress_loss),
            c.packets_sent,
            c.packets_received,
        );
        if let Some(m) = &s.media {
            stats.push_str(match m.transport {
                MediaTransport::Udp => " · через UDP",
                MediaTransport::Tunnel => " · через TCP (WebSocket)",
            });
        }
    }
    let mut devices = String::new();
    let mut mic_level = 0.0;
    let mut speaking = false;
    let mut dsp_available = false;
    let mut error = s.last_error.clone().unwrap_or_default();
    if let Some(e) = &s.engine {
        let dev = |d: &Option<turbo_pancake_media_audio::engine::ActiveDevice>| {
            d.as_ref().map_or("нет устройства".to_string(), |d| {
                format!(
                    "{} ({} Гц{})",
                    d.name,
                    d.sample_rate,
                    if d.fallback { ", замена" } else { "" }
                )
            })
        };
        devices = format!("Микрофон: {} · Вывод: {}", dev(&e.input), dev(&e.output));
        mic_level = if e.muted {
            0.0
        } else {
            meter_position(e.mic_db)
        };
        speaking = e.speaking;
        if let Some(gate) = e.voice_gate {
            devices.push_str(&format!(" · активация голосом от {gate:.0} дБ"));
        }
        dsp_available = e.dsp_available;
        if error.is_empty()
            && let Some(le) = &e.last_error
        {
            error = format!("аудио: {le}");
        }
    }
    CallView {
        connection,
        in_room: s.room.is_some(),
        title,
        detail,
        stats,
        devices,
        error,
        mic_level,
        speaking,
        dsp_available,
        members: s.members.iter().map(member_view).collect(),
    }
}

/// One `TURBO_PANCAKE_CALL key=value …` line for logs and scripted trials.
pub fn log_line(s: &Snapshot, elapsed_ms: u128) -> String {
    let server = match &s.server {
        ServerStatus::Connecting => "connecting",
        ServerStatus::Connected => "connected",
        ServerStatus::Disconnected(_) => "disconnected",
    };
    let state = s.media.as_ref().map_or("none".to_string(), |m| {
        match &m.state {
            CallState::Failed(_) => "failed",
            CallState::Negotiating => "negotiating",
            CallState::Connecting => "connecting",
            CallState::Connected => "connected",
            CallState::Reconnecting => "reconnecting",
            CallState::Closed => "closed",
        }
        .to_string()
    });
    let mut line = format!(
        "TURBO_PANCAKE_CALL t_ms={elapsed_ms} server={server} room={} joined={} others={} media={state} transport={} media_sessions={}",
        s.room.as_deref().unwrap_or("-"),
        s.joined,
        s.members.len(),
        s.media.as_ref().map_or("-", |m| match m.transport {
            MediaTransport::Udp => "udp",
            MediaTransport::Tunnel => "websocket",
        }),
        s.media_sessions,
    );
    if let Some(c) = &s.call {
        line.push_str(&format!(
            " rtt_ms={} sent={} recv={} egress_loss={} ingress_loss={} connected_after_ms={} max_loop_gap_ms={:.1} server_addr={}",
            c.rtt_ms.unwrap_or(-1.0),
            c.packets_sent,
            c.packets_received,
            c.egress_loss.unwrap_or(-1.0),
            c.ingress_loss.unwrap_or(-1.0),
            c.connected_after_ms.map_or(-1, |m| m as i64),
            c.max_loop_gap_ms,
            c.remote_addr.map_or("-".to_string(), |a| a.to_string()),
        ));
    }
    line.push_str(&format!(
        " speaking_members={}",
        s.members.iter().filter(|m| m.speaking()).count()
    ));
    if let Some(e) = &s.engine {
        line.push_str(&format!(
            " streams={} speaking={} vad_db={} frames_gated={} mic_db={:.1}",
            e.peers.len(),
            e.speaking,
            e.voice_gate
                .map_or("off".to_string(), |g| format!("{g:.0}")),
            e.frames_gated,
            e.mic_db,
        ));
        line.push_str(&format!(
            " mic={:?} mic_hz={} out={:?} out_hz={} mic_peak={:.3} muted={} frames_sent={} in_xruns={} out_xruns={} reopens={} ns={} aec={} dsp={} erle_db={}",
            e.input.as_ref().map_or("", |d| d.name.as_str()),
            e.input.as_ref().map_or(0, |d| d.sample_rate),
            e.output.as_ref().map_or("", |d| d.name.as_str()),
            e.output.as_ref().map_or(0, |d| d.sample_rate),
            e.mic_peak,
            e.muted,
            e.frames_sent,
            e.input_xruns,
            e.output_xruns,
            e.reopens,
            e.dsp.noise_suppression,
            e.dsp.echo_cancellation,
            e.dsp_available,
            e.dsp_stats.erle_db.unwrap_or(-1.0),
        ));
        if let Some(p) = e.peers.first() {
            let r = &p.stats;
            line.push_str(&format!(
                " played={} fec={} concealed={} late={} underruns={} decode_errors={} target_ms={} rx_peak={:.3}",
                r.jitter.played,
                r.jitter.fec,
                r.jitter.concealed,
                r.jitter.late,
                r.jitter.underruns,
                r.decode_errors,
                r.jitter.target_delay_ms,
                r.last_peak,
            ));
        }
    }
    if let Some(err) = &s.last_error {
        line.push_str(&format!(" error={err:?}"));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::MediaStatus;

    #[test]
    fn titles_follow_the_call_lifecycle() {
        let mut s = Snapshot::default();
        assert_eq!(render(&s).title, "Не в звонке");
        assert!(!render(&s).in_room);
        s.room = Some("r1".into());
        assert_eq!(render(&s).detail, "Вход в комнату…");
        s.joined = true;
        assert_eq!(render(&s).detail, "Подключение к медиасерверу…");
        s.media = Some(MediaStatus {
            transport: MediaTransport::Tunnel,
            state: CallState::Connected,
        });
        assert_eq!(render(&s).detail, "Ожидание собеседника…");
        s.members = vec![Member {
            peer_id: "p2".into(),
            name: "Боб".into(),
            ssrc: Some(7),
            volume: 0.5,
            stream: None,
        }];
        let v = render(&s);
        assert_eq!(v.title, "Комната «r1»");
        assert_eq!(v.detail, "В звонке: Боб");
        assert!(v.in_room);
        assert_eq!(v.members.len(), 1);
        assert_eq!(v.members[0].volume_pct, 50.0);
        assert_eq!(v.members[0].info, "нет звука");
        assert!(!v.members[0].speaking);
        s.server = ServerStatus::Disconnected("x".into());
        assert_eq!(render(&s).connection, "Нет связи: x");
        let line = log_line(&s, 5);
        assert!(line.starts_with("TURBO_PANCAKE_CALL t_ms=5 server=disconnected room=r1"));
        assert!(line.contains("media=connected transport=websocket"));
        assert!(line.contains("speaking_members=0"));
        assert_eq!(meter_position(-100.0), 0.0);
        assert_eq!(meter_position(-30.0), 0.5);
        assert_eq!(meter_position(3.0), 1.0);
    }
}
