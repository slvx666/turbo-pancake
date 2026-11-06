//! Text the UI shows for a [`Snapshot`] (formatted on the session thread, so the UI
//! thread only assigns strings) and the machine-readable `--call-log` line.

use turbo_pancake_media_transport::call::CallState;

use crate::banner::CallLink;
use crate::session::{MediaTransport, Member, ServerStatus, Snapshot, WatchLayer};
use turbo_pancake_protocol::v1::LayerReason;

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

/// One active share of another member.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShareRowView {
    pub peer_id: String,
    pub name: String,
    /// "1920×1080 · 60 к/с".
    pub info: String,
    pub watching: bool,
    /// Its preview stream while we receive it (key of the preview pictures).
    pub preview_ssrc: Option<u32>,
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
    pub sharing: bool,
    /// Our share's settings and sending stats in one line.
    pub share_info: String,
    pub shares: Vec<ShareRowView>,
    /// Peer id of the watched share ("" = none).
    pub watching: String,
    /// The watched share's receive stats in one line.
    pub viewer_info: String,
    /// Which layer of the watched share flows and why.
    pub layer_info: String,
    /// Previews: on/off, the page and how many pages, decoder stats in one line.
    pub previews_visible: bool,
    pub preview_first: usize,
    pub preview_page: usize,
    pub preview_pages: usize,
    pub preview_info: String,
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
    let share_info = match (&s.share_config, &s.sharing) {
        (Some(c), Some(st)) => {
            format!(
                "Вы показываете экран: {}×{} → {}×{} · {:.0} к/с · {:.1} из {:.1} Мбит/с{} · кодек {}{}{}",
                st.source_width,
                st.source_height,
                st.width,
                st.height,
                st.encoded_fps,
                st.bitrate_kbps / 1000.0,
                f64::from(st.bitrate_kbps_target) / 1000.0,
                if c.adaptive {
                    format!(" (авто, макс. {}×{} · {} к/с)", c.width, c.height, c.fps)
                } else {
                    " (вручную)".to_string()
                },
                st.encoder,
                if st.hardware {
                    ""
                } else {
                    " (программный)"
                },
                if st.preview_width > 0 {
                    format!(" · превью {}×{}", st.preview_width, st.preview_height)
                } else {
                    String::new()
                },
            ) + &match &s.share_audio {
                Some(a) => format!(
                    " · звук компьютера (без звука мессенджера), пик {:.2}",
                    a.peak
                ),
                None => " · без звука компьютера".to_string(),
            }
        }
        _ => String::new(),
    };
    let watching = s.watching.clone().unwrap_or_default();
    let viewer_info = match &s.viewer {
        Some(v) if v.frames_shown == 0 => match &v.error {
            Some(e) => e.clone(),
            None => "Ожидание первого кадра…".to_string(),
        },
        Some(v) => format!(
            "{}×{} · {:.0} к/с · декодер {}{} · потеряно кадров {}",
            v.width,
            v.height,
            v.fps,
            if v.hardware_decoder {
                "аппаратный"
            } else {
                "программный"
            },
            if v.latency_ms_p50 > 0.0 {
                format!(" · задержка {:.0} мс", v.latency_ms_p50)
            } else {
                String::new()
            },
            v.frames_dropped,
        ),
        None => String::new(),
    };
    let shares: Vec<ShareRowView> = s
        .shares
        .iter()
        .map(|sh| ShareRowView {
            peer_id: sh.peer_id.clone(),
            name: sh.name.clone(),
            info: format!("{}×{} · {} к/с", sh.width, sh.height, sh.fps),
            watching: watching == sh.peer_id,
            preview_ssrc: sh.preview_ssrc,
        })
        .collect();
    let requested = match s.watch_layer {
        WatchLayer::Auto => "авто",
        WatchLayer::High => "высокое",
        WatchLayer::Low => "низкое",
    };
    let layer_info = match (&s.watching, &s.layer) {
        (None, _) => String::new(),
        (Some(_), None) => format!("Качество: {requested} · ожидание ответа сервера"),
        (Some(_), Some(l)) => {
            let got = if l.high {
                "высокое"
            } else {
                "низкое (превью)"
            };
            let why = match l.reason {
                LayerReason::Network => " — сеть теряет пакеты",
                LayerReason::Admission => " — лимит сервера на трансляции в высоком качестве",
                LayerReason::SingleLayer => " — отправитель передаёт одно качество",
                _ => "",
            };
            format!("Качество: {got} (запрошено: {requested}){why}")
        }
    };
    // Previews: the shares that are not watched, paged by `max_previews`.
    let candidates = s
        .shares
        .iter()
        .filter(|sh| sh.has_preview && s.watching.as_deref() != Some(sh.peer_id.as_str()))
        .count();
    let per_page = s.max_previews.max(1);
    let preview_pages = candidates.div_ceil(per_page);
    let preview_info = match (&s.previews, s.previews_visible) {
        (_, false) => "Миниатюры скрыты: их потоки не принимаются".to_string(),
        (Some(p), true) => format!(
            "Миниатюры: {} потоков · {:.0} кадров/с · декодирование {:.1} мс · занятость {:.0} %",
            p.streams,
            p.fps,
            p.decode_ms_p50,
            p.busy * 100.0
        ),
        (None, true) => String::new(),
    };
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
        sharing: s.sharing.is_some(),
        share_info,
        shares,
        watching,
        viewer_info,
        layer_info,
        previews_visible: s.previews_visible,
        preview_first: s.preview_first,
        preview_page: s.preview_first / per_page,
        preview_pages,
        preview_info,
    }
}

/// The call's link for the connection banner: lost signaling or a recovering media
/// session while in a room is `Reconnecting`; an ended/failed call is the call view's.
pub fn call_link(s: &Snapshot) -> CallLink {
    if s.room.is_none() {
        return CallLink::None;
    }
    if matches!(s.server, ServerStatus::Disconnected(_)) {
        return CallLink::Reconnecting;
    }
    if !s.joined {
        return CallLink::Connecting;
    }
    match s.media.as_ref().map(|m| &m.state) {
        Some(CallState::Connected) => CallLink::Connected,
        Some(CallState::Reconnecting) => CallLink::Reconnecting,
        Some(CallState::Closed | CallState::Failed(_)) => CallLink::None,
        None | Some(CallState::Negotiating | CallState::Connecting) => CallLink::Connecting,
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
    // Per member: name:frames played:concealed %:speaking (0/1), "-" before their stream.
    let per_member: Vec<String> = s
        .members
        .iter()
        .map(|m| match &m.stream {
            Some(p) => format!(
                "{}:{}:{:.1}:{}",
                m.name.replace([' ', ','], "_"),
                p.stats.jitter.played,
                p.stats.concealed_ratio() * 100.0,
                u8::from(p.stats.speaking)
            ),
            None => format!("{}:-", m.name.replace([' ', ','], "_")),
        })
        .collect();
    line.push_str(&format!(" members={}", per_member.join(",")));
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
    if let Some(st) = &s.sharing {
        line.push_str(&format!(
            " share={}x{}@{} share_src={}x{} encoder={:?} hw={} captured={} submitted={} repeat={} encoded={} enc_fps={:.1} share_kbps={:.0} keyframes={} kf_requests={} enc_busy={} undelivered={} enc_ms_p50={:.2} enc_ms_p95={:.2} cap_to_enc_ms_p50={:.2} cap_to_enc_ms_p95={:.2}",
            st.width,
            st.height,
            st.fps,
            st.source_width,
            st.source_height,
            st.encoder,
            st.hardware,
            st.captured_frames,
            st.submitted_frames,
            st.submitted_repeat,
            st.encoded_frames,
            st.encoded_fps,
            st.bitrate_kbps,
            st.keyframes,
            st.keyframe_requests,
            st.dropped_encoder_busy,
            st.dropped_delivery,
            st.encode_ms_p50,
            st.encode_ms_p95,
            st.capture_to_encoded_ms_p50,
            st.capture_to_encoded_ms_p95,
        ));
    }
    if let Some(a) = &s.share_audio {
        line.push_str(&format!(
            " share_audio_frames={} share_audio_silent={} share_audio_peak={:.3} share_audio_excludes_self={}",
            a.frames, a.silent_frames, a.peak, a.excludes_self
        ));
    }
    if let Some(p) = &s.watched_audio {
        line.push_str(&format!(
            " watched_audio_played={} watched_audio_concealed={:.1} watched_audio_peak={:.3}",
            p.stats.jitter.played,
            p.stats.concealed_ratio() * 100.0,
            p.stats.last_peak
        ));
    }
    if let Some(st) = &s.sharing {
        line.push_str(&format!(
            " share_target_kbps={} reconfigs={} preview={}x{} preview_frames={} preview_kbps={:.0}",
            st.bitrate_kbps_target,
            st.reconfigurations,
            st.preview_width,
            st.preview_height,
            st.preview_frames,
            st.preview_bitrate_kbps,
        ));
    }
    if let Some(c) = &s.call {
        line.push_str(&format!(
            " video_sent={} video_pkts_sent={} video_pkts_recv={} video_nacks_sent={} video_nacks_recv={} video_paced_drops={} video_egress_loss={:.3} pacing_kbps={} preview_sent={} preview_pkts_recv={}",
            c.video_frames_sent,
            c.video_packets_sent,
            c.video_packets_received,
            c.video_nacks_sent,
            c.video_nacks_received,
            c.video_frames_dropped_pacing,
            c.video_egress_loss.unwrap_or(-1.0),
            c.video_pacing_bps / 1000,
            c.preview_frames_sent,
            c.preview_packets_received,
        ));
    }
    if s.watching.is_some() {
        line.push_str(&format!(
            " watch_layer={:?} layer={} layer_reason={:?}",
            s.watch_layer,
            match &s.layer {
                Some(l) if l.high => "high",
                Some(_) => "low",
                None => "-",
            },
            s.layer.as_ref().map(|l| l.reason)
        ));
    }
    if let Some(p) = &s.previews {
        line.push_str(&format!(
            " previews={} preview_fps={:.1} preview_decoded={} preview_dropped={} preview_decode_ms={:.2} preview_busy={:.3}",
            p.streams, p.fps, p.frames_decoded, p.frames_dropped, p.decode_ms_p50, p.busy
        ));
    }
    line.push_str(&format!(
        " shares={} watching={}",
        s.shares.len(),
        s.watching.as_deref().unwrap_or("-")
    ));
    if let Some(v) = &s.viewer {
        line.push_str(&format!(
            " view={}x{} view_fps={:.1} shown={} decoded={} assembled={} view_dropped={} decode_errors={} view_kf_requests={} decode_ms_p50={:.2} latency_ms_p50={:.1} latency_ms_p95={:.1} hw_decoder={}",
            v.width,
            v.height,
            v.fps,
            v.frames_shown,
            v.frames_decoded,
            v.frames_assembled,
            v.frames_dropped,
            v.decode_errors,
            v.keyframe_requests,
            v.decode_ms_p50,
            v.latency_ms_p50,
            v.latency_ms_p95,
            v.hardware_decoder,
        ));
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
        assert_eq!(call_link(&s), CallLink::None);
        assert_eq!(render(&s).title, "Не в звонке");
        assert!(!render(&s).in_room);
        s.room = Some("r1".into());
        assert_eq!(render(&s).detail, "Вход в комнату…");
        s.joined = true;
        assert_eq!(render(&s).detail, "Подключение к медиасерверу…");
        assert_eq!(call_link(&s), CallLink::Connecting);
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
        assert_eq!(call_link(&s), CallLink::Connected);
        s.server = ServerStatus::Disconnected("x".into());
        assert_eq!(render(&s).connection, "Нет связи: x");
        assert_eq!(call_link(&s), CallLink::Reconnecting);
        let line = log_line(&s, 5);
        assert!(line.starts_with("TURBO_PANCAKE_CALL t_ms=5 server=disconnected room=r1"));
        assert!(line.contains("media=connected transport=websocket"));
        assert!(line.contains("speaking_members=0"));
        assert!(line.contains(" members=Боб:-"), "{line}");
        assert!(line.contains(" shares=0 watching=-"), "{line}");
        s.shares = vec![crate::session::ShareView {
            peer_id: "p2".into(),
            name: "Боб".into(),
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 8000,
            has_preview: true,
            preview_ssrc: None,
        }];
        s.watching = Some("p2".into());
        let v = render(&s);
        assert_eq!(v.shares.len(), 1);
        assert_eq!(v.shares[0].info, "1920×1080 · 60 к/с");
        assert!(v.shares[0].watching);
        assert!(!v.sharing && v.share_info.is_empty());
        assert_eq!(meter_position(-100.0), 0.0);
        assert_eq!(meter_position(-30.0), 0.5);
        assert_eq!(meter_position(3.0), 1.0);
    }
}
