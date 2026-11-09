//! `sfu` scenario: N media members through the release server's SFU, measuring server
//! CPU and forwarding throughput for a number of members talking at once.
//!
//! Each member is a real client minus the audio engine: signaling over the WebSocket
//! (hello, join, `MediaOffer` over UDP, `TrackPublished` → `expect_source`) and a
//! `CallDriver` (ICE + DTLS-SRTP, str0m) on its own loopback UDP socket. Talking members
//! send one synthetic Opus-sized payload every 20 ms; everybody counts what arrives. The
//! server runs in its own process, so its CPU time is measured apart from the members.
//!
//! `--accounts` (S8 step 3): the members are signed-in accounts in one group call
//! (`call_accounts`) instead of anonymous peers in a named room.

use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;
use turbo_pancake_media_transport::call::{
    AudioOut, CallConfig, CallDriver, CallEvent, CallState, LocalSignal, MediaLink, Role,
};
use turbo_pancake_protocol::v1::{self, client_envelope::Body as C, server_envelope::Body as S};

use crate::call_accounts::{self, Join};
use crate::{MIB, host_info, mem, spawn_lines, wait_line, with_child_diag, ws_send};

const ROOM: &str = "sfu-bench";
const FRAME: Duration = Duration::from_millis(20);
/// Per-packet bytes on the wire besides the payload, server → member: IPv4 + UDP (28),
/// RTP header (12), header extensions negotiated by the SFU — audio level, abs-send-time,
/// transport-cc in one-byte form (16), SRTP auth tag (10).
const WIRE_OVERHEAD: u64 = 28 + 12 + 16 + 10;

/// Shared counters of all members.
#[derive(Default)]
struct Counters {
    connected: AtomicUsize,
    tracks: AtomicUsize,
    rx_packets: AtomicU64,
    rx_bytes: AtomicU64,
    tx_packets: AtomicU64,
    /// Members that received anything in the current phase (bit per member, ≤ 64).
    heard_mask: AtomicU64,
}

/// Account mode of the media benches: `None` = anonymous room peers.
pub struct Accounts {
    pub concurrency: usize,
    pub database_url: Option<String>,
}

pub async fn sfu(
    server_exe: &std::path::Path,
    members: usize,
    talkers: &[usize],
    seconds: u64,
    payload: usize,
    accounts: Option<Accounts>,
) -> anyhow::Result<serde_json::Value> {
    if !(2..=64).contains(&members) {
        bail!("--members must be 2..=64");
    }
    turbo_pancake_media_transport::init_crypto();
    let (mut child, rx, tail, database) = match &accounts {
        Some(a) => crate::messenger::start_server(server_exe, a.database_url.as_deref(), &[])?,
        None => {
            let mut cmd = Command::new(server_exe);
            cmd.args(["--bind", "127.0.0.1:0"]).env("RUST_LOG", "warn");
            let (c, r, t) = spawn_lines(cmd)?;
            (c, r, t, "none (anonymous room)")
        }
    };
    let concurrency = accounts.as_ref().map(|a| a.concurrency);
    let result = run(&child, &rx, members, talkers, seconds, payload, concurrency).await;
    let result = result.map_err(|e| with_child_diag(e, &mut child, &tail));
    let _ = child.kill();
    let _ = child.wait();
    let (summary, phases) = result?;
    Ok(json!({
        "scenario": "sfu",
        "label": if concurrency.is_some() {
            format!("{members}m-accounts")
        } else {
            format!("{members}m")
        },
        "host": host_info(),
        "params": {
            "members": members, "talkers": talkers, "seconds_per_phase": seconds,
            "payload_bytes": payload, "wire_overhead_bytes": WIRE_OVERHEAD,
            "accounts": concurrency.is_some(), "database": database,
        },
        "phases": phases,
        "summary": summary,
    }))
}

/// Accumulated CPU time of a process in milliseconds.
/// Process CPU time (user + kernel) in ms. Windows reads `GetProcessTimes`: sysinfo's
/// accumulated time there is derived from sampled usage and was off by orders of
/// magnitude between refreshes (found in S8 step 2).
pub(crate) fn cpu_ms(sys: &mut sysinfo::System, pid: u32) -> anyhow::Result<u64> {
    #[cfg(windows)]
    {
        let _ = sys;
        crate::mem::process_cpu_ms(pid)
    }
    #[cfg(not(windows))]
    {
        sysinfo_cpu_ms(sys, pid)
    }
}

#[cfg(not(windows))]
fn sysinfo_cpu_ms(sys: &mut sysinfo::System, pid: u32) -> anyhow::Result<u64> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate};
    let pid = Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_cpu(),
    );
    Ok(sys
        .process(pid)
        .context("server process gone")?
        .accumulated_cpu_time())
}

#[allow(clippy::too_many_lines)]
async fn run(
    child: &Child,
    rx: &mpsc::Receiver<String>,
    members: usize,
    talkers: &[usize],
    seconds: u64,
    payload: usize,
    accounts: Option<usize>,
) -> anyhow::Result<(serde_json::Value, serde_json::Value)> {
    let (addr, joins, setup) = match accounts {
        Some(concurrency) => {
            let (addr, http) = crate::messenger::ready(rx, concurrency).await?;
            let (joins, setup) = call_accounts::setup(&http, members, concurrency).await?;
            (addr, joins, setup)
        }
        None => {
            let line = wait_line(rx, "TURBO_PANCAKE_LISTENING", Duration::from_secs(10))?;
            let addr = line
                .split_whitespace()
                .nth(1)
                .context("no addr")?
                .to_owned();
            let joins = (0..members)
                .map(|i| Join::Room {
                    name: format!("m{i:02}"),
                    room: ROOM.into(),
                })
                .collect::<Vec<_>>();
            (addr, joins, serde_json::Value::Null)
        }
    };
    let pid = child.id();
    let mem_start = mem::sample(pid)?;
    let counters = Arc::new(Counters::default());
    let stop = Arc::new(AtomicBool::new(false));
    let active = Arc::new(AtomicUsize::new(0));

    let mut feeds = Vec::with_capacity(members);
    let mut tasks = Vec::with_capacity(members);
    for (i, join) in joins.iter().enumerate() {
        let (tx, rx) = sync_channel::<AudioOut>(50);
        feeds.push(tx);
        tasks.push(tokio::spawn(member(
            addr.clone(),
            join.clone(),
            i,
            rx,
            counters.clone(),
            stop.clone(),
        )));
    }
    let feeder = std::thread::spawn({
        let (stop, active, counters) = (stop.clone(), active.clone(), counters.clone());
        move || feed(feeds, payload, &stop, &active, &counters)
    });

    // Every member connected and announced to every other one.
    let deadline = Instant::now() + Duration::from_secs(30);
    while counters.connected.load(Ordering::Relaxed) < members
        || counters.tracks.load(Ordering::Relaxed) < members * (members - 1)
    {
        if Instant::now() > deadline {
            bail!(
                "setup: {} of {members} connected, {} of {} tracks",
                counters.connected.load(Ordering::Relaxed),
                counters.tracks.load(Ordering::Relaxed),
                members * (members - 1)
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let mut sys = sysinfo::System::new().ok().context("sysinfo unavailable")?;
    let mut phases = Vec::new();
    for &k in talkers {
        let k = k.min(members);
        active.store(k, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_secs(2)).await; // streams start, buffers settle
        counters.heard_mask.store(0, Ordering::Relaxed);
        let (c0, t0) = (cpu_ms(&mut sys, pid)?, Instant::now());
        let (rx0, b0, tx0) = (
            counters.rx_packets.load(Ordering::Relaxed),
            counters.rx_bytes.load(Ordering::Relaxed),
            counters.tx_packets.load(Ordering::Relaxed),
        );
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        let (c1, wall) = (cpu_ms(&mut sys, pid)?, t0.elapsed().as_secs_f64());
        let rx_packets = counters.rx_packets.load(Ordering::Relaxed) - rx0;
        let rx_bytes = counters.rx_bytes.load(Ordering::Relaxed) - b0;
        let tx_packets = counters.tx_packets.load(Ordering::Relaxed) - tx0;
        let heard = counters.heard_mask.load(Ordering::Relaxed).count_ones() as usize;
        // Each talker reaches every other member.
        let expected = (k * (members - 1)) as f64 * wall / FRAME.as_secs_f64();
        let cpu_pct = (c1 - c0) as f64 / 10.0 / wall;
        let out_pps = rx_packets as f64 / wall;
        let wire_mbit = (rx_bytes + rx_packets * WIRE_OVERHEAD) as f64 * 8.0 / wall / 1e6;
        let mem = mem::sample(pid)?;
        phases.push(json!({
            "talkers": k,
            "forwarded_streams": k * (members - 1),
            "wall_s": wall,
            "server_cpu_pct_of_one_core": cpu_pct,
            "server_cpu_us_per_forwarded_packet": if rx_packets > 0 {
                (c1 - c0) as f64 * 1000.0 / rx_packets as f64
            } else { 0.0 },
            "in_pps": tx_packets as f64 / wall,
            "out_pps": out_pps,
            "delivered_ratio": if expected > 0.0 { rx_packets as f64 / expected } else { 1.0 },
            "members_receiving": heard,
            "egress_mbit_s_estimated": wire_mbit,
            "server_working_set_mib": mem.working_set_bytes as f64 / MIB,
        }));
        println!("{}", phases.last().unwrap());
    }

    stop.store(true, Ordering::Relaxed);
    let _ = feeder.join();
    for t in tasks {
        let _ = t.await;
    }
    let summary = json!({
        "members": members,
        "accounts": accounts.is_some(),
        "setup": setup,
        "server_working_set_mib_start": mem_start.working_set_bytes as f64 / MIB,
        "phases": phases.iter().map(|p| json!({
            "talkers": p["talkers"],
            "cpu_pct": p["server_cpu_pct_of_one_core"],
            "out_pps": p["out_pps"],
            "delivered": p["delivered_ratio"],
            "egress_mbit_s": p["egress_mbit_s_estimated"],
        })).collect::<Vec<_>>(),
    });
    Ok((summary, serde_json::Value::Array(phases)))
}

/// Sends one frame per 20 ms to the first `active` members.
fn feed(
    feeds: Vec<SyncSender<AudioOut>>,
    payload: usize,
    stop: &AtomicBool,
    active: &AtomicUsize,
    counters: &Counters,
) {
    let mut seq = 0u16;
    let mut ts = 0u32;
    let mut next = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let k = active.load(Ordering::Relaxed);
        for f in feeds.iter().take(k) {
            let frame = AudioOut {
                seq,
                timestamp: ts,
                payload: vec![0xF8; payload],
            };
            if f.try_send(frame).is_ok() {
                counters.tx_packets.fetch_add(1, Ordering::Relaxed);
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

async fn member(
    addr: String,
    join: Join,
    index: usize,
    outgoing: mpsc::Receiver<AudioOut>,
    counters: Arc<Counters>,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let (mut ws, _, room) = call_accounts::enter(&addr, &join).await?;

    let mut cc = CallConfig::new(Role::Offerer, "127.0.0.1:0".parse()?);
    cc.advertise = Some("127.0.0.1".parse()?);
    let rx_counters = counters.clone();
    let bit = 1u64 << index;
    let (driver, events) = CallDriver::start_with(cc, MediaLink::Udp, outgoing, move |p| {
        rx_counters.rx_packets.fetch_add(1, Ordering::Relaxed);
        rx_counters
            .rx_bytes
            .fetch_add(p.payload.len() as u64, Ordering::Relaxed);
        rx_counters.heard_mask.fetch_or(bit, Ordering::Relaxed);
    })?;
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(e) = events.recv() {
            if ev_tx.send(e).is_err() {
                break;
            }
        }
    });

    let mut connected = false;
    // Keeps the signaling connection alive (the server evicts silent peers).
    let mut tick = tokio::time::interval(Duration::from_secs(5));
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
                            driver.expect_source(t.ssrc)?;
                            counters.tracks.fetch_add(1, Ordering::Relaxed);
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
                _ = tick.tick() => {
                    ws_send(&mut ws, 1, C::Ping(v1::Ping { client_time_us: 0 })).await?;
                }
            }
        }
        Ok(())
    }
    .await;
    driver.stop();
    let _ = ws.send(Message::Close(None)).await;
    result
}
