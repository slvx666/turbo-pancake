//! `messenger` scenario (S8 step 1): N signed-in WebSocket sessions with presence on the
//! release server and a real PostgreSQL.
//!
//! Accounts are created through the real register API (server started with
//! `--load-test-limits`, which it allows on loopback only), split into groups of up to
//! `--group-size` members so every user has contacts. Every account then holds one
//! authenticated WebSocket with the client's heartbeat. Measured phases:
//!
//! * register / group creation / connect latency;
//! * presence churn: a member disconnects and reconnects; time until every online
//!   contact got `PresenceChanged` (offline, then online);
//! * group fan-out: a member posts to a 100-member group over HTTP; time until every
//!   other member's connection got `MessageCreated`;
//! * hold: all sessions idle with heartbeats; server memory and CPU, ping RTT, dropped
//!   connections;
//! * silent clients: some connections stop sending (socket kept open, like a crashed
//!   or frozen client); the server must mark them offline after its heartbeat timeout,
//!   and an HTTP snapshot of every other member must show no stale "online".
//!
//! Loopback numbers are the server's own cost, not network latency. The throw-away
//! PostgreSQL of `crates/pg-test` runs with `fsync=off`, so database writes are cheaper
//! than on a durable server: the target-server run (owner-assisted) passes
//! `--database-url` of the real database.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::{Child, Command};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail, ensure};
use futures_util::{StreamExt, stream};
use prost::Message as _;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use turbo_pancake_protocol::v1::{self, client_envelope::Body as C, server_envelope::Body as S};

use crate::{MIB, Stats, host_info, mem, spawn_lines, stats, wait_line, with_child_diag};

pub struct Params {
    pub users: usize,
    pub group_size: usize,
    pub churn: usize,
    pub messages: usize,
    pub hold_seconds: u64,
    pub silent: usize,
    pub concurrency: usize,
    pub database_url: Option<String>,
}

/// Generous bound for one fan-out to complete; a miss is a failure of the run.
const FANOUT_TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const PASSWORD: &str = "load test password";

struct Account {
    username: String,
    user_id: String,
    token: String,
    group: usize,
}

/// What a connection task reports to the scenario.
pub(crate) enum EvKind {
    Presence {
        user: String,
        online: bool,
    },
    Message {
        body: String,
        conversation: String,
        seq: i64,
        change_seq: i64,
    },
    Pong {
        sent_us: u64,
    },
    Closed,
}

pub(crate) struct Ev {
    pub at: Instant,
    pub conn: usize,
    pub kind: EvKind,
}

pub(crate) enum Cmd {
    Close,
    /// Stop sending anything (heartbeats included) but keep the socket open.
    Silence,
    Ping(u64),
}

pub(crate) struct Conn {
    pub cmd: mpsc::UnboundedSender<Cmd>,
    pub task: tokio::task::JoinHandle<()>,
}

type Ws = crate::Ws;

pub async fn messenger(server_exe: &Path, p: Params) -> anyhow::Result<serde_json::Value> {
    ensure!(p.users >= 4, "--users must be at least 4");
    ensure!(
        (2..=100).contains(&p.group_size),
        "--group-size must be 2..=100 (spec §7: groups of up to 100)"
    );
    ensure!(p.concurrency >= 1, "--concurrency must be at least 1");
    let (mut child, rx, tail, db_kind) = start_server(server_exe, p.database_url.as_deref())?;
    let result = run(&child, &rx, &p).await;
    let result = result.map_err(|e| with_child_diag(e, &mut child, &tail));
    let _ = child.kill();
    let _ = child.wait();
    let (summary, raw) = result?;
    Ok(json!({
        "scenario": "messenger",
        "label": format!("{}u", p.users),
        "host": host_info(),
        "params": {
            "users": p.users, "group_size": p.group_size, "churn": p.churn,
            "messages": p.messages, "hold_seconds": p.hold_seconds, "silent": p.silent,
            "concurrency": p.concurrency, "database": db_kind,
        },
        "raw": raw,
        "summary": summary,
    }))
}

pub(crate) struct Http {
    pub client: reqwest::Client,
    pub base: String,
}

impl Http {
    pub async fn call<R: prost::Message + Default>(
        &self,
        method: reqwest::Method,
        path: &str,
        token: Option<&str>,
        body: Option<Vec<u8>>,
    ) -> anyhow::Result<R> {
        let (status, bytes) = self.raw(method, path, token, body).await?;
        if !status.is_success() {
            let err = v1::ApiError::decode(bytes.as_ref()).unwrap_or_default();
            bail!("{path}: HTTP {status}: {}", err.message);
        }
        Ok(R::decode(bytes.as_ref())?)
    }

    /// One request; returns the status and the body without judging the status.
    pub async fn raw(
        &self,
        method: reqwest::Method,
        path: &str,
        token: Option<&str>,
        body: Option<Vec<u8>>,
    ) -> anyhow::Result<(reqwest::StatusCode, Vec<u8>)> {
        let mut req = self.client.request(method, format!("{}{path}", self.base));
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        if let Some(b) = body {
            req = req
                .header("content-type", turbo_pancake_protocol::API_CONTENT_TYPE)
                .body(b);
        }
        let resp = req.send().await?;
        let status = resp.status();
        Ok((status, resp.bytes().await?.to_vec()))
    }
}

pub(crate) fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Starts the release server on loopback with load-test limits and the given (or a
/// throw-away) PostgreSQL. Returns the child, its stdout lines, the stderr tail and the
/// database kind for the report.
pub(crate) fn start_server(
    server_exe: &Path,
    database_url: Option<&str>,
) -> anyhow::Result<(
    Child,
    std_mpsc::Receiver<String>,
    crate::StderrTail,
    &'static str,
)> {
    let (db_url, db_kind) = match database_url {
        Some(u) => (u.to_owned(), "external"),
        None => (
            turbo_pancake_pg_test::database_url()
                .context("PostgreSQL binaries not found (TURBO_PANCAKE_PG_BIN)")?,
            "pg-test (fsync=off)",
        ),
    };
    let mut cmd = Command::new(server_exe);
    cmd.args([
        "--bind",
        "127.0.0.1:0",
        "--no-media-udp",
        "--load-test-limits",
    ])
    .env("TURBO_PANCAKE_DATABASE_URL", &db_url)
    .env("RUST_LOG", "warn");
    let (child, rx, tail) = spawn_lines(cmd)?;
    Ok((child, rx, tail, db_kind))
}

/// Waits for the listening line and for the messenger routes (migrations run first).
pub(crate) async fn ready(
    rx: &std_mpsc::Receiver<String>,
    pool_idle: usize,
) -> anyhow::Result<(String, Http)> {
    let line = wait_line(rx, "TURBO_PANCAKE_LISTENING", Duration::from_secs(30))?;
    let addr = line
        .split_whitespace()
        .nth(1)
        .context("no addr")?
        .to_owned();
    let http = Http {
        client: reqwest::Client::builder()
            .use_preconfigured_tls((*turbo_pancake_tls::client_config(None)?).clone())
            .pool_max_idle_per_host(pool_idle)
            .build()?,
        base: format!("http://{addr}"),
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let ok = http
            .client
            .get(format!("{}/v1/version", http.base))
            .send()
            .await
            .ok()
            .filter(|r| r.status().is_success());
        if let Some(r) = ok {
            let v: serde_json::Value = serde_json::from_slice(&r.bytes().await?)?;
            ensure!(v["messenger"] == true, "server runs without the messenger");
            break;
        }
        ensure!(Instant::now() < deadline, "server did not become ready");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok((addr, http))
}

async fn run(
    child: &Child,
    rx: &std_mpsc::Receiver<String>,
    p: &Params,
) -> anyhow::Result<(serde_json::Value, serde_json::Value)> {
    let pid = child.id();
    let (addr, http) = ready(rx, p.concurrency).await?;
    let mem_start = mem::sample(pid)?;
    let mut sys = sysinfo::System::new().ok().context("sysinfo unavailable")?;
    let cpu_start = crate::sfu::cpu_ms(&mut sys, pid)?;

    // --- accounts ---------------------------------------------------------------
    let run_tag = crate::unix_now() % 100_000;
    let t = Instant::now();
    let registered: Vec<(usize, anyhow::Result<(v1::AuthResponse, f64)>)> =
        stream::iter(0..p.users)
            .map(|i| {
                let http = &http;
                async move {
                    let username = format!("lt{run_tag}_{i}");
                    let req = v1::RegisterRequest {
                        username,
                        password: PASSWORD.into(),
                        display_name: format!("Load {i}"),
                        device_name: "turbo-pancake-bench".into(),
                    };
                    let t0 = Instant::now();
                    let r = http
                        .call::<v1::AuthResponse>(
                            reqwest::Method::POST,
                            "/v1/auth/register",
                            None,
                            Some(req.encode_to_vec()),
                        )
                        .await
                        .map(|a| (a, ms(t0.elapsed())));
                    (i, r)
                }
            })
            .buffer_unordered(p.concurrency)
            .collect()
            .await;
    let register_total_ms = ms(t.elapsed());
    let mut accounts: Vec<Option<Account>> = (0..p.users).map(|_| None).collect();
    let mut register_ms = Vec::with_capacity(p.users);
    for (i, r) in registered {
        let (auth, took) = r.with_context(|| format!("register user {i}"))?;
        let user = auth.user.context("AuthResponse without user")?;
        register_ms.push(took);
        accounts[i] = Some(Account {
            username: user.username,
            user_id: user.user_id,
            token: auth.token,
            group: i / p.group_size,
        });
    }
    let accounts: Vec<Account> = accounts.into_iter().map(Option::unwrap).collect();
    let mem_registered = mem::sample(pid)?;

    // --- groups -----------------------------------------------------------------
    let groups: Vec<Vec<usize>> = (0..p.users)
        .collect::<Vec<_>>()
        .chunks(p.group_size)
        .map(<[usize]>::to_vec)
        .filter(|g| g.len() >= 2)
        .collect();
    let mut group_ids = Vec::with_capacity(groups.len());
    let mut group_create_ms = Vec::new();
    for (gi, members) in groups.iter().enumerate() {
        let creator = &accounts[members[0]];
        let req = v1::CreateGroupRequest {
            title: format!("Load group {gi}"),
            usernames: members[1..]
                .iter()
                .map(|&m| accounts[m].username.clone())
                .collect(),
        };
        let t0 = Instant::now();
        let conv: v1::Conversation = http
            .call(
                reqwest::Method::POST,
                "/v1/conversations/group",
                Some(&creator.token),
                Some(req.encode_to_vec()),
            )
            .await
            .with_context(|| format!("create group {gi}"))?;
        group_create_ms.push(ms(t0.elapsed()));
        ensure!(
            conv.members.len() == members.len(),
            "group {gi}: {} members, expected {}",
            conv.members.len(),
            members.len()
        );
        group_ids.push(conv.conversation_id);
    }
    // A leftover single user (users % group_size == 1) has no group and no contacts.
    let group_of = |i: usize| -> Option<usize> {
        let g = accounts[i].group;
        (g < groups.len() && groups[g].contains(&i)).then_some(g)
    };

    // --- connect ----------------------------------------------------------------
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<Ev>();
    let user_index: HashMap<String, usize> = accounts
        .iter()
        .enumerate()
        .map(|(i, a)| (a.user_id.clone(), i))
        .collect();
    let t = Instant::now();
    let connected: Vec<(usize, anyhow::Result<(Conn, f64)>)> = stream::iter(0..p.users)
        .map(|i| {
            let (addr, token, ev_tx) = (&addr, accounts[i].token.clone(), ev_tx.clone());
            async move { (i, connect(addr, i, &token, ev_tx).await) }
        })
        .buffer_unordered(p.concurrency.max(16))
        .collect()
        .await;
    let connect_total_ms = ms(t.elapsed());
    let mut conns: Vec<Option<Conn>> = (0..p.users).map(|_| None).collect();
    let mut connect_ms = Vec::with_capacity(p.users);
    for (i, r) in connected {
        let (c, took) = r.with_context(|| format!("connect user {i}"))?;
        connect_ms.push(took);
        conns[i] = Some(c);
    }
    let mut conns: Vec<Conn> = conns.into_iter().map(Option::unwrap).collect();
    // Let the connect-storm presence fan-out finish, then forget it.
    let storm = settle(&mut ev_rx, Duration::from_millis(1500)).await;
    let storm_presence = storm
        .iter()
        .filter(|e| matches!(e.kind, EvKind::Presence { .. }))
        .count();
    let mem_connected = mem::sample(pid)?;

    // --- presence churn -----------------------------------------------------------
    let mut offline_last_ms = Vec::new();
    let mut online_last_ms = Vec::new();
    let mut presence_each_ms = Vec::new();
    let mut churn_missed = 0usize;
    let churners: Vec<usize> = (0..p.churn)
        .map(|k| (k * 7919 + 3) % p.users)
        .filter(|&i| group_of(i).is_some())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    for &u in &churners {
        let g = group_of(u).expect("churner has a group");
        let watchers: HashSet<usize> = groups[g].iter().copied().filter(|&m| m != u).collect();
        let uid = accounts[u].user_id.clone();

        let t0 = Instant::now();
        let _ = conns[u].cmd.send(Cmd::Close);
        let (last, each, missed) = await_presence(&mut ev_rx, &uid, false, &watchers, t0).await;
        offline_last_ms.extend(last);
        presence_each_ms.extend(each);
        churn_missed += missed;
        let old = std::mem::replace(
            &mut conns[u],
            Conn {
                cmd: mpsc::unbounded_channel().0,
                task: tokio::spawn(async {}),
            },
        );
        let _ = old.task.await;

        let t0 = Instant::now();
        let (c, _) = connect(&addr, u, &accounts[u].token, ev_tx.clone()).await?;
        conns[u] = c;
        let (last, each, missed) = await_presence(&mut ev_rx, &uid, true, &watchers, t0).await;
        online_last_ms.extend(last);
        presence_each_ms.extend(each);
        churn_missed += missed;
    }

    // --- group message fan-out ------------------------------------------------------
    let mut fanout_last_ms = Vec::new();
    let mut fanout_each_ms = Vec::new();
    let mut send_ms = Vec::new();
    let mut fanout_missed = 0usize;
    let big = groups
        .iter()
        .enumerate()
        .max_by_key(|(_, g)| g.len())
        .map(|(gi, _)| gi)
        .context("no group")?;
    for k in 0..p.messages {
        let members = &groups[big];
        let sender = members[k % members.len()];
        let body = format!("load message {run_tag}-{k}");
        let watchers: HashSet<usize> = members.iter().copied().filter(|&m| m != sender).collect();
        let req = v1::SendMessageRequest {
            client_nonce: format!("lt-{run_tag}-{k}"),
            body: body.clone(),
            reply_to_id: String::new(),
        };
        let t0 = Instant::now();
        let _: v1::ChatMessage = http
            .call(
                reqwest::Method::POST,
                &format!("/v1/conversations/{}/messages", group_ids[big]),
                Some(&accounts[sender].token),
                Some(req.encode_to_vec()),
            )
            .await
            .with_context(|| format!("send message {k}"))?;
        send_ms.push(ms(t0.elapsed()));
        let mut seen = HashSet::new();
        let deadline = t0 + FANOUT_TIMEOUT;
        let mut last = None;
        while seen.len() < watchers.len() {
            let Some(ev) = recv_until(&mut ev_rx, deadline).await else {
                break;
            };
            if let EvKind::Message { body: b, .. } = &ev.kind
                && *b == body
                && watchers.contains(&ev.conn)
                && seen.insert(ev.conn)
            {
                let d = ms(ev.at - t0);
                fanout_each_ms.push(d);
                last = Some(d);
            }
        }
        fanout_missed += watchers.len() - seen.len();
        fanout_last_ms.extend(last);
    }

    // --- hold ---------------------------------------------------------------------
    let _ = settle(&mut ev_rx, Duration::from_millis(500)).await;
    let mut hold_ws = Vec::new();
    let mut hold_private = Vec::new();
    let mut rtt_ms = Vec::new();
    let clock = Instant::now();
    let c0 = crate::sfu::cpu_ms(&mut sys, pid)?;
    let t_hold = Instant::now();
    let mut closed_during_hold = 0usize;
    for s in 0..p.hold_seconds {
        let probe = (s as usize * 37) % p.users;
        let sent = clock.elapsed().as_micros() as u64;
        let _ = conns[probe].cmd.send(Cmd::Ping(sent));
        let until = Instant::now() + Duration::from_secs(1);
        while let Some(ev) = recv_until(&mut ev_rx, until).await {
            match ev.kind {
                EvKind::Pong { sent_us } if sent_us == sent => {
                    rtt_ms.push((clock.elapsed().as_micros() as u64 - sent_us) as f64 / 1000.0);
                }
                EvKind::Closed => closed_during_hold += 1,
                _ => {}
            }
        }
        let m = mem::sample(pid)?;
        hold_ws.push(m.working_set_bytes as f64 / MIB);
        if let Some(pb) = m.private_bytes {
            hold_private.push(pb as f64 / MIB);
        }
    }
    let hold_wall = t_hold.elapsed().as_secs_f64();
    let c1 = crate::sfu::cpu_ms(&mut sys, pid)?;
    let hold_cpu_pct = (c1 - c0) as f64 / 10.0 / hold_wall.max(0.001);

    // --- silent clients (heartbeat timeout) -----------------------------------------
    let silent: Vec<usize> = (0..p.silent.min(p.users / 2))
        .map(|k| (k * 104_729 + 11) % p.users)
        .filter(|&i| group_of(i).is_some())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let t0 = Instant::now();
    for &i in &silent {
        let _ = conns[i].cmd.send(Cmd::Silence);
    }
    let silent_ids: HashSet<String> = silent
        .iter()
        .map(|&i| accounts[i].user_id.clone())
        .collect();
    let silent_set: HashSet<usize> = silent.iter().copied().collect();
    // Every live contact of each silent user must see it go offline.
    let mut expected: HashSet<(usize, String)> = HashSet::new();
    for &i in &silent {
        let g = group_of(i).expect("silent user has a group");
        for &m in &groups[g] {
            if m != i && !silent_set.contains(&m) {
                expected.insert((m, accounts[i].user_id.clone()));
            }
        }
    }
    let expected_n = expected.len();
    let mut silent_detect_ms = Vec::new();
    let mut silent_closed = 0usize;
    let deadline = t0 + Duration::from_secs(90);
    while !expected.is_empty() {
        let Some(ev) = recv_until(&mut ev_rx, deadline).await else {
            break;
        };
        match &ev.kind {
            EvKind::Presence {
                user,
                online: false,
            } if silent_ids.contains(user) => {
                if expected.remove(&(ev.conn, user.clone())) {
                    silent_detect_ms.push(ms(ev.at - t0));
                }
            }
            EvKind::Closed if silent_set.contains(&ev.conn) => silent_closed += 1,
            _ => {}
        }
    }
    let silent_missed = expected.len();
    let _ = settle(&mut ev_rx, Duration::from_millis(300)).await;

    // HTTP snapshot of every live member: presence of each contact must match the truth.
    let mut snapshot_wrong = 0usize;
    let mut snapshot_checked = 0usize;
    let live: Vec<usize> = (0..p.users).filter(|i| !silent_set.contains(i)).collect();
    let snapshots: Vec<anyhow::Result<v1::ConversationList>> = stream::iter(live.iter().copied())
        .map(|i| {
            let (http, token) = (&http, accounts[i].token.clone());
            async move {
                http.call(
                    reqwest::Method::GET,
                    "/v1/conversations",
                    Some(&token),
                    None,
                )
                .await
            }
        })
        .buffer_unordered(p.concurrency.max(16))
        .collect()
        .await;
    for list in snapshots {
        for conv in list?.conversations {
            for m in conv.members {
                let Some(u) = m.user else { continue };
                let Some(&idx) = user_index.get(&u.user_id) else {
                    continue;
                };
                snapshot_checked += 1;
                if u.online == silent_set.contains(&idx) {
                    snapshot_wrong += 1;
                }
            }
        }
    }

    // --- shutdown -------------------------------------------------------------------
    for c in &conns {
        let _ = c.cmd.send(Cmd::Close);
    }
    for c in conns.drain(..) {
        let _ = tokio::time::timeout(Duration::from_secs(5), c.task).await;
    }
    let mem_end = mem::sample(pid)?;
    let cpu_total_ms = crate::sfu::cpu_ms(&mut sys, pid)? - cpu_start;

    let wsmib = |m: &mem::MemSample| m.working_set_bytes as f64 / MIB;
    let pmib = |m: &mem::MemSample| m.private_bytes.map(|b| b as f64 / MIB);
    let per_session_kib = (mem_connected.working_set_bytes as f64
        - mem_registered.working_set_bytes as f64)
        / p.users as f64
        / 1024.0;
    let pass = churn_missed == 0
        && fanout_missed == 0
        && closed_during_hold == 0
        && silent_missed == 0
        && snapshot_wrong == 0;
    let st = |v: &Vec<f64>| -> Option<Stats> { stats(v.clone()) };
    let summary = json!({
        "pass": pass,
        "users": p.users,
        "groups": groups.len(),
        "largest_group": groups[big].len(),
        "register_ms": st(&register_ms),
        "register_total_s": register_total_ms / 1000.0,
        "group_create_ms": st(&group_create_ms),
        "connect_ms": st(&connect_ms),
        "connect_all_s": connect_total_ms / 1000.0,
        "connect_storm_presence_events": storm_presence,
        "presence": {
            "flips": churners.len() * 2,
            "offline_last_contact_ms": st(&offline_last_ms),
            "online_last_contact_ms": st(&online_last_ms),
            "per_contact_ms": st(&presence_each_ms),
            "missed_deliveries": churn_missed,
        },
        "group_fanout": {
            "messages": p.messages,
            "receivers_per_message": groups[big].len() - 1,
            "send_http_ms": st(&send_ms),
            "last_receiver_ms": st(&fanout_last_ms),
            "per_receiver_ms": st(&fanout_each_ms),
            "missed_deliveries": fanout_missed,
        },
        "hold": {
            "seconds": p.hold_seconds,
            "server_cpu_pct_of_one_core": hold_cpu_pct,
            "server_cpu_ms": c1 - c0,
            "ping_rtt_ms": st(&rtt_ms),
            "connections_closed": closed_during_hold,
            "working_set_mib": st(&hold_ws),
            "private_mib": st(&hold_private),
        },
        "silent_clients": {
            "silent": silent.len(),
            "expected_offline_deliveries": expected_n,
            "detected_after_ms": st(&silent_detect_ms),
            "missed": silent_missed,
            "closed_by_server": silent_closed,
        },
        "snapshot_presence": { "checked": snapshot_checked, "wrong": snapshot_wrong },
        "server_working_set_mib": {
            "start": wsmib(&mem_start), "registered": wsmib(&mem_registered),
            "connected": wsmib(&mem_connected), "end": wsmib(&mem_end),
        },
        "server_private_mib": {
            "start": pmib(&mem_start), "connected": pmib(&mem_connected), "end": pmib(&mem_end),
        },
        "server_ws_per_session_kib": per_session_kib,
        // Whole run (registrations' Argon2id included): a check that the counter moves.
        "server_cpu_s_total": cpu_total_ms as f64 / 1000.0,
    });
    let raw = json!({
        "register_ms": register_ms, "connect_ms": connect_ms,
        "offline_last_contact_ms": offline_last_ms, "online_last_contact_ms": online_last_ms,
        "fanout_last_receiver_ms": fanout_last_ms, "send_http_ms": send_ms,
        "hold_working_set_mib": hold_ws, "ping_rtt_ms": rtt_ms,
        "silent_detected_after_ms": silent_detect_ms,
    });
    Ok((summary, raw))
}

/// Waits until every watcher got `PresenceChanged{uid, online}`. Returns the last
/// arrival, every arrival (ms since `t0`) and the number of watchers that missed it.
async fn await_presence(
    rx: &mut mpsc::UnboundedReceiver<Ev>,
    uid: &str,
    online: bool,
    watchers: &HashSet<usize>,
    t0: Instant,
) -> (Option<f64>, Vec<f64>, usize) {
    let mut seen = HashSet::new();
    let mut each = Vec::new();
    let mut last = None;
    let deadline = t0 + FANOUT_TIMEOUT;
    while seen.len() < watchers.len() {
        let Some(ev) = recv_until(rx, deadline).await else {
            break;
        };
        if let EvKind::Presence { user, online: o } = &ev.kind
            && user == uid
            && *o == online
            && watchers.contains(&ev.conn)
            && seen.insert(ev.conn)
        {
            let d = ms(ev.at - t0);
            each.push(d);
            last = Some(d);
        }
    }
    (last, each, watchers.len() - seen.len())
}

pub(crate) async fn recv_until(
    rx: &mut mpsc::UnboundedReceiver<Ev>,
    deadline: Instant,
) -> Option<Ev> {
    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), rx.recv())
        .await
        .ok()
        .flatten()
}

/// Collects events until none arrived for `quiet`.
pub(crate) async fn settle(rx: &mut mpsc::UnboundedReceiver<Ev>, quiet: Duration) -> Vec<Ev> {
    let mut out = Vec::new();
    while let Ok(Some(ev)) = tokio::time::timeout(quiet, rx.recv()).await {
        out.push(ev);
    }
    out
}

/// Opens a signed-in WebSocket (Hello with the token → Welcome) and starts its task.
pub(crate) async fn connect(
    addr: &str,
    idx: usize,
    token: &str,
    ev_tx: mpsc::UnboundedSender<Ev>,
) -> anyhow::Result<(Conn, f64)> {
    let t0 = Instant::now();
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/v1/signal")).await?;
    crate::ws_send(
        &mut ws,
        1,
        C::Hello(v1::Hello {
            client_version: "turbo-pancake-bench".into(),
            display_name: String::new(),
            auth_token: token.into(),
        }),
    )
    .await?;
    let heartbeat = loop {
        match crate::ws_recv(&mut ws).await? {
            S::Welcome(w) => {
                ensure!(w.user.is_some(), "Welcome without the signed-in user");
                break Duration::from_millis(u64::from(w.heartbeat_interval_ms.max(1000)));
            }
            S::Error(e) => bail!("hello refused: {}", e.message),
            _ => continue,
        }
    };
    let took = ms(t0.elapsed());
    let (cmd, cmd_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(conn_task(ws, idx, heartbeat, cmd_rx, ev_tx));
    Ok((Conn { cmd, task }, took))
}

async fn conn_task(
    mut ws: Ws,
    idx: usize,
    heartbeat: Duration,
    mut cmd: mpsc::UnboundedReceiver<Cmd>,
    ev: mpsc::UnboundedSender<Ev>,
) {
    let mut silent = false;
    let mut tick = tokio::time::interval(heartbeat);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;
    let mut rid = 10u64;
    let report = |kind| {
        let _ = ev.send(Ev {
            at: Instant::now(),
            conn: idx,
            kind,
        });
    };
    loop {
        tokio::select! {
            msg = ws.next() => {
                let Some(Ok(msg)) = msg else { break };
                let Message::Binary(b) = msg else { continue };
                let Ok(env) = turbo_pancake_protocol::decode_server(&b) else { continue };
                match env.body {
                    Some(S::PresenceChanged(pc)) => report(EvKind::Presence {
                        user: pc.user_id,
                        online: pc.online,
                    }),
                    Some(S::MessageCreated(m)) => {
                        if let Some(msg) = m.message {
                            report(EvKind::Message {
                                body: msg.body,
                                conversation: msg.conversation_id,
                                seq: msg.server_sequence,
                                change_seq: msg.change_seq,
                            });
                        }
                    }
                    Some(S::Pong(p)) => report(EvKind::Pong { sent_us: p.client_time_us }),
                    Some(S::SessionRevoked(_)) => break,
                    _ => {}
                }
            }
            _ = tick.tick(), if !silent => {
                rid += 1;
                let ping = C::Ping(v1::Ping { client_time_us: 0 });
                if crate::ws_send(&mut ws, rid, ping).await.is_err() {
                    break;
                }
            }
            c = cmd.recv() => match c {
                None | Some(Cmd::Close) => {
                    let _ = ws.close(None).await;
                    // Drain until the server's close so its unregister has happened.
                    let _ = tokio::time::timeout(Duration::from_secs(5), async {
                        while let Some(Ok(_)) = ws.next().await {}
                    })
                    .await;
                    return;
                }
                Some(Cmd::Silence) => silent = true,
                Some(Cmd::Ping(t)) => {
                    rid += 1;
                    if crate::ws_send(&mut ws, rid, C::Ping(v1::Ping { client_time_us: t }))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            },
        }
    }
    report(EvKind::Closed);
}
