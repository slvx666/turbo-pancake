//! `sfu-screen` scenarios (S4, spec §5.3 / §13: tested separately, never mixed in one
//! claim): screen share forwarding through the release server's SFU with real call
//! drivers (ICE + DTLS-SRTP over loopback UDP), synthetic H.264-shaped payloads.
//!
//! * `publishers` — N members share (high + preview layer); 2 viewers each watch one high
//!   stream and show every share as a preview.
//! * `fanout` — 1 member shares; N viewers watch its high layer.
//! * `grid` — N members share; every member shows all the others as previews.
//!
//! S8 step 3 additions: `--accounts` runs the members as signed-in accounts in one
//! group call (`call_accounts`) instead of anonymous room peers; `--talkers` members
//! also send voice (Opus-sized frames every 20 ms) during the share, so the run shows
//! whether audio keeps flowing; `--max-high-egress-mbps` gives the server its high-layer
//! budget: viewers beyond it must get the preview layer on their main picture with the
//! reason `ADMISSION`, and the report checks each viewer against the layer it was told.
//!
//! Every access unit starts with an IDR NAL, so the SFU can start or switch a stream at
//! any frame (payload content is irrelevant to forwarding). The server runs in its own
//! process: its CPU time is measured apart from the members (which run here).

use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;
use turbo_pancake_media_transport::call::{
    AudioOut, CallConfig, CallDriver, CallEvent, CallState, LocalSignal, MediaLink, Role, VideoIo,
};
use turbo_pancake_media_transport::video::VideoOut;
use turbo_pancake_protocol::v1::{self, client_envelope::Body as C, server_envelope::Body as S};

use crate::call_accounts::{self, Join};
use crate::sfu::cpu_ms;
use crate::{MIB, host_info, mem, spawn_lines, wait_line, with_child_diag, ws_send};

const ROOM: &str = "sfu-screen";
/// Wire bytes per packet besides the payload (see `sfu.rs`).
const WIRE_OVERHEAD: u64 = 28 + 12 + 16 + 10;
/// Voice frame: 100 B every 20 ms ≈ Opus at 40 kbit/s.
const VOICE_BYTES: usize = 100;
const FRAME: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Scenario {
    Publishers,
    Fanout,
    Grid,
}

#[derive(Debug, Clone)]
pub struct Params {
    pub scenario: Scenario,
    /// Publishers (`publishers`, `grid`) or viewers (`fanout`).
    pub n: usize,
    pub seconds: u64,
    pub high_kbps: u32,
    pub high_fps: u32,
    pub preview_kbps: u32,
    pub preview_fps: u32,
    /// Members that also talk (the first ones; in `fanout` the sharer is member 0).
    pub talkers: usize,
    /// The server's high-layer budget (`--max-high-egress-mbps`).
    pub max_high_egress_mbps: Option<u32>,
    pub accounts: bool,
    pub concurrency: usize,
    pub database_url: Option<String>,
}

#[derive(Clone, Copy)]
struct Rates {
    high_kbps: u32,
    high_fps: u32,
    preview_kbps: u32,
    preview_fps: u32,
}

/// Layer a viewer was last told (`ScreenSubscription`): 0 none, 1 high, 2 low.
const LAYER_NONE: u8 = 0;
const LAYER_HIGH: u8 = 1;
const LAYER_LOW: u8 = 2;

#[derive(Default)]
struct Counters {
    connected: AtomicUsize,
    screen_tracks: AtomicUsize,
    rx_main: AtomicU64,
    rx_preview: AtomicU64,
    rx_bytes: AtomicU64,
    rx_audio: AtomicU64,
    audio_sent: AtomicU64,
}

/// Per member.
#[derive(Default)]
struct PerMember {
    /// Packets sent on the main / preview layer (refreshed every 0.5 s).
    sent_main: AtomicU64,
    sent_preview: AtomicU64,
    /// Main-picture packets and payload bytes received.
    rx_main: AtomicU64,
    rx_main_bytes: AtomicU64,
    layer: AtomicU8,
    /// `LayerReason` as its protobuf number.
    reason: AtomicU8,
}

enum Ctl {
    Share,
    Watch(String),
    Previews(Vec<String>),
}

struct MemberRole {
    shares: bool,
    watches: Option<usize>,
    previews: Vec<usize>,
}

fn roles(p: &Params) -> Vec<MemberRole> {
    match p.scenario {
        Scenario::Publishers => {
            let mut r: Vec<MemberRole> = (0..p.n)
                .map(|_| MemberRole {
                    shares: true,
                    watches: None,
                    previews: Vec::new(),
                })
                .collect();
            for j in 0..2 {
                r.push(MemberRole {
                    shares: false,
                    watches: Some(j % p.n),
                    previews: (0..p.n).filter(|i| *i != j % p.n).collect(),
                });
            }
            r
        }
        Scenario::Fanout => std::iter::once(MemberRole {
            shares: true,
            watches: None,
            previews: Vec::new(),
        })
        .chain((0..p.n).map(|_| MemberRole {
            shares: false,
            watches: Some(0),
            previews: Vec::new(),
        }))
        .collect(),
        Scenario::Grid => (0..p.n)
            .map(|i| MemberRole {
                shares: true,
                watches: None,
                previews: (0..p.n).filter(|j| *j != i).collect(),
            })
            .collect(),
    }
}

pub async fn sfu_screen(
    server_exe: &std::path::Path,
    p: Params,
) -> anyhow::Result<serde_json::Value> {
    if !(1..=60).contains(&p.n) {
        bail!("--n must be 1..=60");
    }
    turbo_pancake_media_transport::init_crypto();
    let mut extra: Vec<String> = Vec::new();
    if let Some(m) = p.max_high_egress_mbps {
        extra.extend(["--max-high-egress-mbps".into(), m.to_string()]);
    }
    let (mut child, rx, tail, database) = if p.accounts {
        crate::messenger::start_server(server_exe, p.database_url.as_deref(), &extra)?
    } else {
        let mut cmd = Command::new(server_exe);
        cmd.args(["--bind", "127.0.0.1:0"])
            .args(&extra)
            .env("RUST_LOG", "warn");
        let (c, r, t) = spawn_lines(cmd)?;
        (c, r, t, "none (anonymous room)")
    };
    let result = run(&child, &rx, &p).await;
    let result = result.map_err(|e| with_child_diag(e, &mut child, &tail));
    let _ = child.kill();
    let _ = child.wait();
    let summary = result?;
    let mut label = format!("{:?}-{}", p.scenario, p.n).to_lowercase();
    if p.accounts {
        label.push_str("-accounts");
    }
    if let Some(m) = p.max_high_egress_mbps {
        label.push_str(&format!("-budget{m}"));
    }
    if p.high_kbps != 2000 {
        label.push_str(&format!("-{}k", p.high_kbps));
    }
    Ok(json!({
        "scenario": "sfu-screen",
        "label": label,
        "host": host_info(),
        "params": {
            "scenario": format!("{:?}", p.scenario), "n": p.n, "seconds": p.seconds,
            "high_kbps": p.high_kbps, "high_fps": p.high_fps,
            "preview_kbps": p.preview_kbps, "preview_fps": p.preview_fps,
            "talkers": p.talkers, "max_high_egress_mbps": p.max_high_egress_mbps,
            "accounts": p.accounts, "database": database,
            "wire_overhead_bytes": WIRE_OVERHEAD,
        },
        "summary": summary,
    }))
}

#[allow(clippy::too_many_lines)]
async fn run(
    child: &Child,
    rx: &mpsc::Receiver<String>,
    p: &Params,
) -> anyhow::Result<serde_json::Value> {
    let pid = child.id();
    let roles = roles(p);
    let total = roles.len();
    let (addr, joins, setup) = if p.accounts {
        let (addr, http) = crate::messenger::ready(rx, p.concurrency).await?;
        let (joins, setup) = call_accounts::setup(&http, total, p.concurrency).await?;
        (addr, joins, setup)
    } else {
        let line = wait_line(rx, "TURBO_PANCAKE_LISTENING", Duration::from_secs(10))?;
        let addr = line
            .split_whitespace()
            .nth(1)
            .context("no addr")?
            .to_owned();
        let joins = (0..total)
            .map(|i| Join::Room {
                name: format!("s{i:02}"),
                room: ROOM.into(),
            })
            .collect();
        (addr, joins, serde_json::Value::Null)
    };
    let rates = Rates {
        high_kbps: p.high_kbps,
        high_fps: p.high_fps,
        preview_kbps: p.preview_kbps,
        preview_fps: p.preview_fps,
    };
    let talkers = p.talkers.min(total);
    let counters = Arc::new(Counters::default());
    let per: Arc<Vec<PerMember>> = Arc::new((0..total).map(|_| PerMember::default()).collect());
    let ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![String::new(); total]));
    let stop = Arc::new(AtomicBool::new(false));

    let mut feeds = Vec::new();
    let mut voices = Vec::new();
    let mut ctls = Vec::new();
    let mut tasks = Vec::new();
    for (i, role) in roles.iter().enumerate() {
        let (vtx, vrx) = sync_channel::<VideoOut>(32);
        let (ptx, prx) = sync_channel::<VideoOut>(32);
        if role.shares {
            feeds.push((vtx, ptx));
        }
        let (atx, arx) = sync_channel::<AudioOut>(50);
        if i < talkers {
            voices.push(atx);
        }
        let (ctl_tx, ctl_rx) = tokio::sync::mpsc::unbounded_channel();
        ctls.push(ctl_tx);
        tasks.push(tokio::spawn(member(
            addr.clone(),
            joins[i].clone(),
            i,
            Media {
                video: vrx,
                preview: prx,
                audio: arx,
            },
            rates,
            ctl_rx,
            counters.clone(),
            per.clone(),
            ids.clone(),
            stop.clone(),
        )));
        // Do not open 50 DTLS handshakes in the same millisecond.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let feeder = std::thread::spawn({
        let stop = stop.clone();
        move || feed(feeds, rates, &stop)
    });
    let voice = std::thread::spawn({
        let (stop, counters) = (stop.clone(), counters.clone());
        move || voice_feed(&voices, &stop, &counters)
    });

    let deadline = Instant::now() + Duration::from_secs(60);
    // Every member hears of every other member's screen track.
    while counters.connected.load(Ordering::Relaxed) < total
        || counters.screen_tracks.load(Ordering::Relaxed) < total * (total - 1)
    {
        if Instant::now() > deadline {
            bail!(
                "setup: {} of {total} connected, {} of {} screen tracks",
                counters.connected.load(Ordering::Relaxed),
                counters.screen_tracks.load(Ordering::Relaxed),
                total * (total - 1)
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let peer = |i: usize| ids.lock().unwrap()[i].clone();
    for (i, role) in roles.iter().enumerate() {
        if role.shares {
            let _ = ctls[i].send(Ctl::Share);
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    for (i, role) in roles.iter().enumerate() {
        if let Some(w) = role.watches {
            let _ = ctls[i].send(Ctl::Watch(peer(w)));
        }
        if !role.previews.is_empty() {
            let _ = ctls[i].send(Ctl::Previews(
                role.previews.iter().map(|j| peer(*j)).collect(),
            ));
        }
    }
    // Streams start, layer decisions arrive, buffers settle.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let mut sys = sysinfo::System::new().ok().context("sysinfo unavailable")?;
    let me = std::process::id();
    let mem0 = mem::sample(pid)?;
    let snapshot = || {
        (
            counters.rx_main.load(Ordering::Relaxed),
            counters.rx_preview.load(Ordering::Relaxed),
            counters.rx_bytes.load(Ordering::Relaxed),
            counters.rx_audio.load(Ordering::Relaxed),
            counters.audio_sent.load(Ordering::Relaxed),
            per.iter()
                .map(|s| {
                    (
                        s.sent_main.load(Ordering::Relaxed),
                        s.sent_preview.load(Ordering::Relaxed),
                        s.rx_main.load(Ordering::Relaxed),
                        s.rx_main_bytes.load(Ordering::Relaxed),
                    )
                })
                .collect::<Vec<_>>(),
        )
    };
    let (c0, h0, t0) = (
        cpu_ms(&mut sys, pid)?,
        cpu_ms(&mut sys, me)?,
        Instant::now(),
    );
    let (m0, pv0, b0, a0, as0, s0) = snapshot();
    tokio::time::sleep(Duration::from_secs(p.seconds)).await;
    let (c1, h1, wall) = (
        cpu_ms(&mut sys, pid)?,
        cpu_ms(&mut sys, me)?,
        t0.elapsed().as_secs_f64(),
    );
    let (m1, pv1, b1, a1, as1, s1) = snapshot();
    let mem1 = mem::sample(pid)?;

    // What should have arrived: each subscription gets every packet of the layer it was
    // told it gets (high, or the preview layer on the main picture when downgraded).
    let d = |i: usize| (s1[i].0 - s0[i].0, s1[i].1 - s0[i].1);
    let (mut expected_main, mut expected_preview) = (0u64, 0u64);
    let mut high_streams = 0;
    let mut preview_streams = 0;
    let (mut viewers_high, mut viewers_low, mut viewers_untold) = (0usize, 0usize, 0usize);
    let mut low_admission = 0usize;
    let mut high_kbps_seen = Vec::new();
    let mut low_kbps_seen = Vec::new();
    for (i, r) in roles.iter().enumerate() {
        if let Some(w) = r.watches {
            let got_bytes = s1[i].3 - s0[i].3;
            let kbps = got_bytes as f64 * 8.0 / wall / 1000.0;
            match per[i].layer.load(Ordering::Relaxed) {
                LAYER_LOW => {
                    expected_main += d(w).1;
                    viewers_low += 1;
                    low_kbps_seen.push(kbps);
                    if per[i].reason.load(Ordering::Relaxed)
                        == v1::LayerReason::Admission as i32 as u8
                    {
                        low_admission += 1;
                    }
                }
                l => {
                    expected_main += d(w).0;
                    if l == LAYER_HIGH {
                        viewers_high += 1;
                    } else {
                        viewers_untold += 1;
                    }
                    high_kbps_seen.push(kbps);
                }
            }
            high_streams += 1;
        }
        for j in &r.previews {
            expected_preview += d(*j).1;
            preview_streams += 1;
        }
    }
    let ingress_pkts: u64 = (0..total).map(|i| d(i).0 + d(i).1).sum();
    let (rx_main, rx_preview) = (m1 - m0, pv1 - pv0);
    let rx_pkts = rx_main + rx_preview;
    let rx_audio = a1 - a0;
    // Each talker's frames reach every other member.
    let expected_audio = (as1 - as0) * (total as u64 - 1);
    let ratio = |got: u64, want: u64| {
        if want > 0 {
            got as f64 / want as f64
        } else {
            1.0
        }
    };
    let budget_viewers = p
        .max_high_egress_mbps
        .map(|m| (u64::from(m) * 1000 / u64::from(p.high_kbps.max(1))) as usize);
    let avg = |v: &[f64]| (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64);
    let (delivered_main, delivered_preview, delivered_audio) = (
        ratio(rx_main, expected_main),
        ratio(rx_preview, expected_preview),
        ratio(rx_audio, expected_audio),
    );
    // Loopback: ≥ 98 % of what each subscription was told it gets (counters of the senders
    // are refreshed every 0.5 s, so the window edges cost up to ~1 %).
    let viewers = viewers_high + viewers_low + viewers_untold;
    let layers_ok = viewers_untold == 0
        && match budget_viewers {
            Some(b) => viewers_high == b.min(viewers) && low_admission == viewers - viewers_high,
            None => viewers_low == 0,
        };
    let pass = delivered_main >= 0.98
        && delivered_preview >= 0.98
        && (talkers == 0 || delivered_audio >= 0.98)
        && layers_ok;
    let summary = json!({
        "pass": pass,
        "layers_as_expected": layers_ok,
        "scenario": format!("{:?}", p.scenario),
        "accounts": p.accounts,
        "setup": setup,
        "members": total,
        "publishers": roles.iter().filter(|r| r.shares).count(),
        "talkers": talkers,
        "high_streams_forwarded": high_streams,
        "preview_streams_forwarded": preview_streams,
        "wall_s": wall,
        "server_cpu_pct_of_one_core": (c1 - c0) as f64 / 10.0 / wall,
        "server_cpu_us_per_forwarded_packet": if rx_pkts + rx_audio > 0 {
            (c1 - c0) as f64 * 1000.0 / (rx_pkts + rx_audio) as f64 } else { 0.0 },
        "clients_cpu_pct_of_one_core": (h1 - h0) as f64 / 10.0 / wall,
        "ingress_pps": ingress_pkts as f64 / wall,
        "out_pps": (rx_pkts + rx_audio) as f64 / wall,
        "delivered_main": delivered_main,
        "delivered_preview": delivered_preview,
        "delivered_audio": delivered_audio,
        "audio_packets_expected": expected_audio,
        "viewers": {
            "high": viewers_high,
            "low": viewers_low,
            "low_reason_admission": low_admission,
            "not_told": viewers_untold,
            "budget_allows": budget_viewers,
            "main_kbps_high_avg": avg(&high_kbps_seen),
            "main_kbps_low_avg": avg(&low_kbps_seen),
        },
        "egress_mbit_s_estimated": ((b1 - b0) + rx_audio * VOICE_BYTES as u64
            + (rx_pkts + rx_audio) * WIRE_OVERHEAD) as f64 * 8.0 / wall / 1e6,
        "server_working_set_mib": mem1.working_set_bytes as f64 / MIB,
        "server_working_set_mib_before": mem0.working_set_bytes as f64 / MIB,
    });
    println!("{summary}");
    stop.store(true, Ordering::Relaxed);
    let _ = feeder.join();
    let _ = voice.join();
    for t in tasks {
        let _ = t.await;
    }
    Ok(summary)
}

/// Sends every publisher's layers at their rates (one thread for all).
fn feed(feeds: Vec<(SyncSender<VideoOut>, SyncSender<VideoOut>)>, p: Rates, stop: &AtomicBool) {
    let unit = |kbps: u32, fps: u32| -> Vec<u8> {
        let len = (kbps as usize * 1000 / 8 / fps.max(1) as usize).max(16);
        let mut v = vec![0, 0, 0, 1, 0x65];
        v.resize(len, 0x5A);
        v
    };
    let high = unit(p.high_kbps, p.high_fps);
    let low = unit(p.preview_kbps, p.preview_fps);
    let every = (p.high_fps / p.preview_fps.max(1)).max(1);
    let interval = Duration::from_secs_f64(1.0 / f64::from(p.high_fps));
    let mut next = Instant::now();
    let mut n: u32 = 0;
    while !stop.load(Ordering::Relaxed) {
        let ts = n.wrapping_mul(90_000 / p.high_fps.max(1));
        for (v, pv) in &feeds {
            let _ = v.try_send(VideoOut {
                timestamp: ts,
                data: high.clone(),
            });
            if n.is_multiple_of(every) {
                let _ = pv.try_send(VideoOut {
                    timestamp: ts,
                    data: low.clone(),
                });
            }
        }
        n += 1;
        next += interval;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
}

/// One voice frame per 20 ms for every talker.
fn voice_feed(voices: &[SyncSender<AudioOut>], stop: &AtomicBool, counters: &Counters) {
    let (mut seq, mut ts) = (0u16, 0u32);
    let mut next = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        for v in voices {
            let frame = AudioOut {
                seq,
                timestamp: ts,
                payload: vec![0xF8; VOICE_BYTES],
            };
            if v.try_send(frame).is_ok() {
                counters.audio_sent.fetch_add(1, Ordering::Relaxed);
            }
        }
        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(960);
        next += FRAME;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
}

struct Media {
    video: mpsc::Receiver<VideoOut>,
    preview: mpsc::Receiver<VideoOut>,
    audio: mpsc::Receiver<AudioOut>,
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn member(
    addr: String,
    join: Join,
    index: usize,
    media: Media,
    rates: Rates,
    mut ctl: tokio::sync::mpsc::UnboundedReceiver<Ctl>,
    counters: Arc<Counters>,
    per: Arc<Vec<PerMember>>,
    ids: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let (mut ws, my_id, room) = call_accounts::enter(&addr, &join).await?;
    ids.lock().unwrap()[index] = my_id;
    let mut cc = CallConfig::new(Role::Offerer, "127.0.0.1:0".parse()?);
    cc.advertise = Some("127.0.0.1".parse()?);
    let rxc = counters.clone();
    let rxp = per.clone();
    let (_sa_tx, sa_rx) = sync_channel::<AudioOut>(4);
    let io = VideoIo {
        outgoing: media.video,
        preview: media.preview,
        audio_out: sa_rx,
        on_video: Box::new(move |p| {
            if p.preview {
                rxc.rx_preview.fetch_add(1, Ordering::Relaxed);
            } else {
                rxc.rx_main.fetch_add(1, Ordering::Relaxed);
                rxp[index].rx_main.fetch_add(1, Ordering::Relaxed);
                rxp[index]
                    .rx_main_bytes
                    .fetch_add(p.payload.len() as u64, Ordering::Relaxed);
            }
            rxc.rx_bytes
                .fetch_add(p.payload.len() as u64, Ordering::Relaxed);
        }),
    };
    let rxa = counters.clone();
    let (driver, events) = CallDriver::start_media(
        cc,
        MediaLink::Udp,
        media.audio,
        move |_| {
            rxa.rx_audio.fetch_add(1, Ordering::Relaxed);
        },
        Some(io),
    )?;
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(e) = events.recv() {
            if ev_tx.send(e).is_err() {
                break;
            }
        }
    });
    let mut connected = false;
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let mut pings = 0u32;
    let result: anyhow::Result<()> = async {
        while !stop.load(Ordering::Relaxed) {
            tokio::select! {
                msg = ws.next() => {
                    let Some(msg) = msg else { bail!("server closed") };
                    let Message::Binary(b) = msg? else { continue };
                    let env = turbo_pancake_protocol::decode_server(&b)?;
                    match env.body {
                        Some(S::MediaAnswer(a)) => driver.remote_answer(a.sdp)?,
                        Some(S::TrackPublished(t)) => {
                            let rtx = (t.rtx_ssrc != 0).then_some(t.rtx_ssrc);
                            match t.kind() {
                                v1::TrackKind::Audio => driver.expect_source(t.ssrc)?,
                                v1::TrackKind::Screen => {
                                    driver.expect_video_source(t.ssrc, rtx)?;
                                    counters.screen_tracks.fetch_add(1, Ordering::Relaxed);
                                }
                                v1::TrackKind::ScreenPreview => {
                                    driver.expect_preview_source(t.ssrc, rtx)?;
                                }
                                _ => {}
                            }
                        }
                        Some(S::ScreenSubscription(sub)) => {
                            let layer = match sub.layer() {
                                v1::ScreenLayer::Low => LAYER_LOW,
                                v1::ScreenLayer::High => LAYER_HIGH,
                                v1::ScreenLayer::Auto => LAYER_NONE,
                            };
                            per[index].layer.store(layer, Ordering::Relaxed);
                            per[index].reason.store(sub.reason as u8, Ordering::Relaxed);
                        }
                        Some(S::Error(e)) => bail!("server error: {}", e.message),
                        _ => {}
                    }
                }
                Some(ev) = ev_rx.recv() => match ev {
                    CallEvent::Signal(LocalSignal::Offer(sdp)) => {
                        ws_send(&mut ws, 3, C::MediaOffer(v1::MediaOffer {
                            room_id: room.clone(),
                            sdp,
                            transport: v1::MediaTransport::Udp as i32,
                            dialed_ip: "127.0.0.1".into(),
                        })).await?;
                    }
                    CallEvent::State(CallState::Connected) if !connected => {
                        connected = true;
                        counters.connected.fetch_add(1, Ordering::Relaxed);
                    }
                    CallEvent::State(CallState::Failed(e)) => bail!("member {index}: {e}"),
                    _ => {}
                },
                Some(c) = ctl.recv() => {
                    let body = match c {
                        Ctl::Share => C::ScreenShare(v1::ScreenShare {
                            room_id: room.clone(),
                            active: true,
                            width: 1920,
                            height: 1080,
                            fps: rates.high_fps,
                            // The announced bitrate is what the high budget counts.
                            bitrate_kbps: rates.high_kbps,
                            preview: true,
                        }),
                        Ctl::Watch(peer_id) => C::SubscribeScreen(v1::SubscribeScreen {
                            room_id: room.clone(),
                            peer_id,
                            layer: v1::ScreenLayer::High as i32,
                        }),
                        Ctl::Previews(peer_ids) => C::SubscribePreviews(v1::SubscribePreviews {
                            room_id: room.clone(),
                            peer_ids,
                        }),
                    };
                    ws_send(&mut ws, 4, body).await?;
                }
                _ = tick.tick() => {
                    let st = driver.stats();
                    per[index].sent_main.store(st.video_packets_sent, Ordering::Relaxed);
                    per[index].sent_preview.store(st.preview_packets_sent, Ordering::Relaxed);
                    pings += 1;
                    if pings.is_multiple_of(10) {
                        ws_send(&mut ws, 1, C::Ping(v1::Ping { client_time_us: 0 })).await?;
                    }
                }
            }
        }
        Ok(())
    }
    .await;
    driver.stop();
    let _ = ws.send(Message::Close(None)).await;
    if let Err(e) = &result {
        eprintln!("member {index}: {e:#}");
    }
    result
}
