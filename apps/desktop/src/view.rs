//! Text the UI shows for a [`Snapshot`] (formatted on the session thread, so the UI
//! thread only assigns strings) and the machine-readable `--call-log` line.

use turbo_pancake_media_transport::call::CallState;

use crate::session::{ServerStatus, Snapshot};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CallView {
    pub connection: String,
    pub in_room: bool,
    pub title: String,
    pub detail: String,
    pub stats: String,
    pub devices: String,
    pub error: String,
    /// Microphone peak 0–1 (after mute).
    pub mic_level: f32,
    pub dsp_available: bool,
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
    let (title, detail) = match (&s.room, &s.remote) {
        (None, _) => (
            "Не в звонке".to_string(),
            "Введите id комнаты и нажмите «Войти»; второй участник входит в ту же комнату."
                .to_string(),
        ),
        (Some(room), None) if !s.joined => {
            (format!("Комната «{room}»"), "Вход в комнату…".to_string())
        }
        (Some(room), None) => (
            format!("Комната «{room}»"),
            "Ожидание собеседника…".to_string(),
        ),
        (Some(room), Some(r)) => {
            let name = if r.name.is_empty() {
                "собеседник"
            } else {
                &r.name
            };
            let state = match &r.state {
                CallState::Negotiating | CallState::Connecting => format!("Соединение с {name}…"),
                CallState::Connected => format!("В звонке с {name}"),
                CallState::Reconnecting => format!("Связь с {name} потеряна, восстановление…"),
                CallState::Closed => "Звонок завершён".to_string(),
                CallState::Failed(e) => format!("Ошибка звонка: {e}"),
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
    }
    if let Some(p) = s.engine.as_ref().and_then(|e| e.peers.first()) {
        let j = &p.stats.jitter;
        stats.push_str(&format!(
            "\nбуфер {} мс · воспроизведено {} · FEC {} · маскировано {} · опоздало {}",
            j.target_delay_ms, j.played, j.fec, j.concealed, j.late
        ));
    }
    let mut devices = String::new();
    let mut mic_level = 0.0;
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
        mic_level = if e.muted { 0.0 } else { e.mic_peak };
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
        dsp_available,
    }
}

/// One `TURBO_PANCAKE_CALL key=value …` line for logs and scripted trials.
pub fn log_line(s: &Snapshot, elapsed_ms: u128) -> String {
    let server = match &s.server {
        ServerStatus::Connecting => "connecting",
        ServerStatus::Connected => "connected",
        ServerStatus::Disconnected(_) => "disconnected",
    };
    let state = s.remote.as_ref().map_or("none".to_string(), |r| {
        match &r.state {
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
        "TURBO_PANCAKE_CALL t_ms={elapsed_ms} server={server} room={} joined={} others={} call={state} calls_started={}",
        s.room.as_deref().unwrap_or("-"),
        s.joined,
        s.others,
        s.calls_started,
    );
    if let Some(c) = &s.call {
        line.push_str(&format!(
            " rtt_ms={} sent={} recv={} egress_loss={} ingress_loss={} connected_after_ms={} max_loop_gap_ms={:.1}",
            c.rtt_ms.unwrap_or(-1.0),
            c.packets_sent,
            c.packets_received,
            c.egress_loss.unwrap_or(-1.0),
            c.ingress_loss.unwrap_or(-1.0),
            c.connected_after_ms.map_or(-1, |m| m as i64),
            c.max_loop_gap_ms,
        ));
    }
    if let Some(e) = &s.engine {
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
    use crate::session::RemotePeer;
    use turbo_pancake_media_transport::call::Role;

    #[test]
    fn titles_follow_the_call_lifecycle() {
        let mut s = Snapshot::default();
        assert_eq!(render(&s).title, "Не в звонке");
        assert!(!render(&s).in_room);
        s.room = Some("r1".into());
        assert_eq!(render(&s).detail, "Вход в комнату…");
        s.joined = true;
        assert_eq!(render(&s).detail, "Ожидание собеседника…");
        s.remote = Some(RemotePeer {
            peer_id: "p".into(),
            name: "Боб".into(),
            role: Role::Offerer,
            state: CallState::Connected,
        });
        let v = render(&s);
        assert_eq!(v.title, "Комната «r1»");
        assert_eq!(v.detail, "В звонке с Боб");
        assert!(v.in_room);
        s.server = ServerStatus::Disconnected("x".into());
        assert_eq!(render(&s).connection, "Нет связи: x");
        let line = log_line(&s, 5);
        assert!(line.starts_with("TURBO_PANCAKE_CALL t_ms=5 server=disconnected room=r1"));
        assert!(line.contains("call=connected"));
    }
}
