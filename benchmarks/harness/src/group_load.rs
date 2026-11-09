//! `group-load` scenario (S8 step 2): 100-member groups under concurrent writes,
//! membership changes and a reconnect storm, on the release server with a real
//! PostgreSQL.
//!
//! Phases:
//!
//! * **load**: in every group `--senders` members post `--messages-per-sender` messages
//!   each over HTTP at the same time (every tenth send is issued twice concurrently with
//!   the same `client_nonce`: one stored message expected), while the creator removes
//!   and re-adds `--churn` other members for `--churn-rounds` rounds. A removed member
//!   probes `send` and `history` (both must be refused) while it is out.
//! * **live check**: every connection's `MessageCreated` events are compared with the
//!   membership windows: a member must get every message committed while it was a
//!   member and none committed while it was removed. The boundary of each change is
//!   bracketed by sequences the bench observed (`lo` = highest sequence of sends that
//!   had completed before the change was requested, `hi` = `last_sequence` in the
//!   change's response); messages inside a bracket may go either way.
//! * **reconnect storm**: every WebSocket closes at once, `--storm-messages` messages per
//!   group are posted while all are offline, every account reconnects at once and
//!   resyncs with `GET /changes?after=<last change_seq it saw>`; nothing may be missing.
//!   One more message per group must then reach every member live.
//! * **history**: the full history of each group is paged with `GET /messages`:
//!   `server_sequence` is 1..=N without gaps or duplicates, N equals the number of
//!   distinct accepted sends, every accepted send is at the sequence it was given and
//!   every sender's sequences increase.
//!
//! The database pool of the server (`/v1/server-stats`) is sampled every 25 ms. Loopback
//! numbers are the server's own cost; `crates/pg-test` runs PostgreSQL with
//! `fsync=off` (the target-server run passes `--database-url`).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, bail, ensure};
use futures_util::{StreamExt, stream};
use prost::Message as _;
use serde_json::json;
use tokio::sync::{mpsc, watch};
use turbo_pancake_protocol::v1;

use crate::messenger::{
    Cmd, Conn, Ev, EvKind, Http, PASSWORD, connect, ms, ready, settle, start_server,
};
use crate::{MIB, host_info, mem, stats, with_child_diag};

pub struct Params {
    pub groups: usize,
    pub group_size: usize,
    pub senders: usize,
    pub messages_per_sender: usize,
    pub send_interval_ms: u64,
    pub churn: usize,
    pub churn_rounds: usize,
    pub storm_messages: usize,
    pub concurrency: usize,
    pub database_url: Option<String>,
}

const GET: reqwest::Method = reqwest::Method::GET;
const POST: reqwest::Method = reqwest::Method::POST;
const DELETE: reqwest::Method = reqwest::Method::DELETE;

struct Account {
    username: String,
    user_id: String,
    token: String,
}

/// One accepted send.
struct Sent {
    group: usize,
    seq: i64,
    message_id: String,
    started: Instant,
}

/// A resync after the storm: account, sequences got, ms taken, pages.
type Resync = (usize, HashSet<i64>, f64, usize);

/// One removal and the re-add that ended it, bracketed by observed sequences.
#[derive(Clone, Copy)]
struct Window {
    rem_lo: i64,
    rem_hi: i64,
    add_lo: i64,
    add_hi: i64,
}

enum Expect {
    Must,
    MustNot,
    Either,
}

fn expect(windows: &[Window], s: i64) -> Expect {
    let mut either = false;
    for w in windows {
        if w.rem_hi < s && s <= w.add_lo {
            return Expect::MustNot;
        }
        if (w.rem_lo < s && s <= w.rem_hi) || (w.add_lo < s && s <= w.add_hi) {
            either = true;
        }
    }
    if either { Expect::Either } else { Expect::Must }
}

#[derive(Default)]
struct ChurnOut {
    windows: HashMap<usize, Vec<Window>>,
    remove_ms: Vec<f64>,
    add_ms: Vec<f64>,
    probes: usize,
    probe_wrong: usize,
    revision_wrong: usize,
}

#[derive(Default)]
struct SendOut {
    sent: Vec<Sent>,
    send_ms: Vec<f64>,
    dup_pairs: usize,
    dup_mismatch: usize,
    order_violations: usize,
}

/// Received events per connection: (conversation, seq) → count, newest change_seq,
/// first arrival per message.
#[derive(Default)]
struct Received {
    count: HashMap<(usize, String, i64), u32>,
    first_at: HashMap<(usize, String, i64), Instant>,
    max_change: HashMap<(usize, String), i64>,
    closed: usize,
}

impl Received {
    fn absorb(&mut self, ev: Ev) {
        match ev.kind {
            EvKind::Message {
                conversation,
                seq,
                change_seq,
                ..
            } => {
                let key = (ev.conn, conversation.clone(), seq);
                *self.count.entry(key.clone()).or_default() += 1;
                self.first_at.entry(key).or_insert(ev.at);
                let m = self.max_change.entry((ev.conn, conversation)).or_default();
                *m = (*m).max(change_seq);
            }
            EvKind::Closed => self.closed += 1,
            _ => {}
        }
    }
}

/// Drains events into `Received` until `stop` flips, then until 1 s of silence.
fn collector(
    mut rx: mpsc::UnboundedReceiver<Ev>,
    mut stop: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<(mpsc::UnboundedReceiver<Ev>, Received)> {
    tokio::spawn(async move {
        let mut got = Received::default();
        loop {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Some(ev) => got.absorb(ev),
                    None => break,
                },
                _ = stop.changed() => break,
            }
        }
        for ev in settle(&mut rx, Duration::from_millis(1000)).await {
            got.absorb(ev);
        }
        (rx, got)
    })
}

/// Samples `/v1/server-stats` every 25 ms: (time, pool in use, pool size).
fn pool_sampler(
    http: Arc<Http>,
    mut stop: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<Vec<(Instant, f64, f64)>> {
    tokio::spawn(async move {
        let mut out = Vec::new();
        let mut tick = tokio::time::interval(Duration::from_millis(25));
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = stop.changed() => break,
            }
            let Ok((status, body)) = http.raw(GET, "/v1/server-stats", None, None).await else {
                continue;
            };
            if !status.is_success() {
                continue;
            }
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) else {
                continue;
            };
            let db = &v["db_pool"];
            if let (Some(u), Some(s)) = (db["in_use"].as_f64(), db["size"].as_f64()) {
                out.push((Instant::now(), u, s));
            }
        }
        out
    })
}

fn pool_summary(
    samples: &[(Instant, f64, f64)],
    from: Instant,
    to: Instant,
    max: f64,
) -> serde_json::Value {
    let in_use: Vec<f64> = samples
        .iter()
        .filter(|(t, _, _)| *t >= from && *t <= to)
        .map(|(_, u, _)| *u)
        .collect();
    let saturated = in_use.iter().filter(|&&u| u >= max).count();
    json!({
        "samples": in_use.len(),
        "in_use": stats(in_use.clone()),
        "saturated_samples": saturated,
        "max_size": max,
    })
}

pub async fn group_load(server_exe: &Path, p: Params) -> anyhow::Result<serde_json::Value> {
    ensure!(p.groups >= 1, "--groups must be at least 1");
    ensure!(
        (4..=100).contains(&p.group_size),
        "--group-size must be 4..=100 (spec §7: groups of up to 100)"
    );
    ensure!(
        p.senders >= 1 && p.senders + p.churn < p.group_size,
        "--senders + --churn must leave the creator out: at most {} together",
        p.group_size - 1
    );
    ensure!(p.concurrency >= 1, "--concurrency must be at least 1");
    let (mut child, rx, tail, db_kind) = start_server(server_exe, p.database_url.as_deref())?;
    let pid = child.id();
    let result = async {
        let (addr, http) = ready(&rx, 64).await?;
        run(pid, &addr, http, &p).await
    }
    .await
    .map_err(|e| with_child_diag(e, &mut child, &tail));
    let _ = child.kill();
    let _ = child.wait();
    let (summary, raw) = result?;
    Ok(json!({
        "scenario": "group-load",
        "label": format!("{}x{}", p.groups, p.group_size),
        "host": host_info(),
        "params": {
            "groups": p.groups, "group_size": p.group_size, "senders": p.senders,
            "messages_per_sender": p.messages_per_sender,
            "send_interval_ms": p.send_interval_ms, "churn": p.churn,
            "churn_rounds": p.churn_rounds, "storm_messages": p.storm_messages,
            "concurrency": p.concurrency, "database": db_kind,
        },
        "raw": raw,
        "summary": summary,
    }))
}

#[allow(clippy::too_many_lines)]
async fn run(
    pid: u32,
    addr: &str,
    http: Http,
    p: &Params,
) -> anyhow::Result<(serde_json::Value, serde_json::Value)> {
    let http = Arc::new(http);
    let users = p.groups * p.group_size;
    let mut sys = sysinfo::System::new().ok().context("sysinfo unavailable")?;
    let pool_max = {
        let (_, body) = http.raw(GET, "/v1/server-stats", None, None).await?;
        let v: serde_json::Value = serde_json::from_slice(&body)?;
        v["db_pool"]["max"]
            .as_f64()
            .context("/v1/server-stats without db_pool")?
    };

    // --- accounts (bounded: each costs one Argon2id hash) ------------------------
    let run_tag = crate::unix_now() % 100_000;
    let registered: Vec<(usize, anyhow::Result<v1::AuthResponse>)> = stream::iter(0..users)
        .map(|i| {
            let http = &http;
            async move {
                let req = v1::RegisterRequest {
                    username: format!("gl{run_tag}_{i}"),
                    password: PASSWORD.into(),
                    display_name: format!("Group load {i}"),
                    device_name: "turbo-pancake-bench".into(),
                };
                let r = http
                    .call(POST, "/v1/auth/register", None, Some(req.encode_to_vec()))
                    .await;
                (i, r)
            }
        })
        .buffer_unordered(p.concurrency)
        .collect()
        .await;
    let mut accounts: Vec<Option<Account>> = (0..users).map(|_| None).collect();
    for (i, r) in registered {
        let auth = r.with_context(|| format!("register user {i}"))?;
        let user = auth.user.context("AuthResponse without user")?;
        accounts[i] = Some(Account {
            username: user.username,
            user_id: user.user_id,
            token: auth.token,
        });
    }
    let accounts: Arc<Vec<Account>> = Arc::new(accounts.into_iter().map(Option::unwrap).collect());

    // --- groups: member 0 creates, 1..=senders send, the last `churn` rotate ------
    let groups: Vec<Vec<usize>> = (0..p.groups)
        .map(|g| (g * p.group_size..(g + 1) * p.group_size).collect())
        .collect();
    let mut group_ids = Vec::new();
    for (gi, members) in groups.iter().enumerate() {
        let req = v1::CreateGroupRequest {
            title: format!("Group load {gi}"),
            usernames: members[1..]
                .iter()
                .map(|&m| accounts[m].username.clone())
                .collect(),
        };
        let conv: v1::Conversation = http
            .call(
                POST,
                "/v1/conversations/group",
                Some(&accounts[members[0]].token),
                Some(req.encode_to_vec()),
            )
            .await
            .with_context(|| format!("create group {gi}"))?;
        ensure!(conv.members.len() == p.group_size, "group {gi} not full");
        group_ids.push(conv.conversation_id);
    }
    let group_ids = Arc::new(group_ids);
    let conv_index: HashMap<String, usize> = group_ids
        .iter()
        .enumerate()
        .map(|(g, id)| (id.clone(), g))
        .collect();
    let group_of = |i: usize| i / p.group_size;

    // --- connect ----------------------------------------------------------------
    let (ev_tx, ev_rx) = mpsc::unbounded_channel::<Ev>();
    let mut conns = connect_all(addr, &accounts, &ev_tx).await?.0;
    let mut ev_rx = ev_rx;
    let _ = settle(&mut ev_rx, Duration::from_millis(1000)).await;

    let (stop_tx, stop_rx) = watch::channel(false);
    let sampler = pool_sampler(http.clone(), stop_rx.clone());
    let mem_before = mem::sample(pid)?;

    // --- load: senders and membership churn in every group at once ---------------
    let (col_stop_tx, col_stop_rx) = watch::channel(false);
    let col = collector(ev_rx, col_stop_rx);
    let committed: Arc<Vec<AtomicI64>> =
        Arc::new((0..p.groups).map(|_| AtomicI64::new(0)).collect());
    let load_start = Instant::now();
    let cpu0 = crate::sfu::cpu_ms(&mut sys, pid)?;
    let mut send_tasks = Vec::new();
    let mut churn_tasks = Vec::new();
    for (g, members) in groups.iter().enumerate() {
        for &s in &members[1..=p.senders] {
            let (http, accounts, group_ids, committed) = (
                http.clone(),
                accounts.clone(),
                group_ids.clone(),
                committed.clone(),
            );
            let (n, interval) = (p.messages_per_sender, p.send_interval_ms);
            send_tasks.push(tokio::spawn(async move {
                sender(
                    &http, &accounts, &group_ids, &committed, g, s, n, interval, run_tag,
                )
                .await
            }));
        }
        let rotating: Vec<usize> = members[p.group_size - p.churn..].to_vec();
        let creator = members[0];
        let (http, accounts, group_ids, committed) = (
            http.clone(),
            accounts.clone(),
            group_ids.clone(),
            committed.clone(),
        );
        let rounds = p.churn_rounds;
        churn_tasks.push(tokio::spawn(async move {
            churn(
                &http, &accounts, &group_ids, &committed, g, creator, &rotating, rounds, run_tag,
            )
            .await
        }));
    }
    let mut sends = SendOut::default();
    for t in send_tasks {
        let o = t.await??;
        sends.sent.extend(o.sent);
        sends.send_ms.extend(o.send_ms);
        sends.dup_pairs += o.dup_pairs;
        sends.dup_mismatch += o.dup_mismatch;
        sends.order_violations += o.order_violations;
    }
    let mut churned = ChurnOut::default();
    for t in churn_tasks {
        let o = t.await??;
        churned.windows.extend(o.windows);
        churned.remove_ms.extend(o.remove_ms);
        churned.add_ms.extend(o.add_ms);
        churned.probes += o.probes;
        churned.probe_wrong += o.probe_wrong;
        churned.revision_wrong += o.revision_wrong;
    }
    let load_end = Instant::now();
    let cpu1 = crate::sfu::cpu_ms(&mut sys, pid)?;
    let _ = col_stop_tx.send(true);
    let (ev_rx, live) = col.await?;
    let mem_load = mem::sample(pid)?;

    // --- live check -------------------------------------------------------------
    let mut live_missed = 0usize;
    let mut live_forbidden = 0usize;
    let mut live_either_got = 0usize;
    let mut live_either_total = 0usize;
    let mut live_dups = 0usize;
    let mut delivery_ms = Vec::new();
    let no_windows: Vec<Window> = Vec::new();
    for m in 0..users {
        let g = group_of(m);
        let conv = &group_ids[g];
        let windows = churned.windows.get(&m).unwrap_or(&no_windows);
        for s in sends.sent.iter().filter(|s| s.group == g) {
            let key = (m, conv.clone(), s.seq);
            let n = live.count.get(&key).copied().unwrap_or(0);
            if n > 1 {
                live_dups += (n - 1) as usize;
            }
            match expect(windows, s.seq) {
                Expect::Must if n == 0 => live_missed += 1,
                Expect::MustNot if n > 0 => live_forbidden += 1,
                Expect::Either => {
                    live_either_total += 1;
                    live_either_got += usize::from(n > 0);
                }
                _ => {}
            }
            if let Some(at) = live.first_at.get(&key) {
                delivery_ms.push(ms(at.saturating_duration_since(s.started)));
            }
        }
    }
    // Every message an account got must belong to its own group.
    let foreign = live
        .count
        .keys()
        .filter(|(c, conv, _)| conv_index.get(conv) != Some(&group_of(*c)))
        .count();

    // --- reconnect storm ----------------------------------------------------------
    let cursors: Vec<i64> = (0..users)
        .map(|m| {
            live.max_change
                .get(&(m, group_ids[group_of(m)].clone()))
                .copied()
                .unwrap_or(0)
        })
        .collect();
    let mut ev_rx = ev_rx;
    for c in &conns {
        let _ = c.cmd.send(Cmd::Close);
    }
    for c in conns.drain(..) {
        let _ = tokio::time::timeout(Duration::from_secs(10), c.task).await;
    }
    let _ = settle(&mut ev_rx, Duration::from_millis(300)).await;
    let mut storm_sent: Vec<(usize, i64)> = Vec::new();
    for (g, members) in groups.iter().enumerate() {
        let posted: Vec<anyhow::Result<v1::ChatMessage>> = stream::iter(0..p.storm_messages)
            .map(|k| {
                let s = members[1 + k % p.senders];
                let req = v1::SendMessageRequest {
                    client_nonce: format!("gls-{run_tag}-{g}-{k}"),
                    body: format!("storm {g}-{k}"),
                    reply_to_id: String::new(),
                };
                let (http, token, path) = (
                    &http,
                    accounts[s].token.clone(),
                    format!("/v1/conversations/{}/messages", group_ids[g]),
                );
                async move {
                    http.call(POST, &path, Some(&token), Some(req.encode_to_vec()))
                        .await
                }
            })
            .buffer_unordered(8)
            .collect()
            .await;
        for m in posted {
            storm_sent.push((g, m.context("storm send")?.server_sequence));
        }
    }
    let storm_start = Instant::now();
    let (col_stop_tx, col_stop_rx) = watch::channel(false);
    let col = collector(ev_rx, col_stop_rx);
    let (new_conns, reconnect_ms) = connect_all(addr, &accounts, &ev_tx).await?;
    conns = new_conns;
    let reconnect_all_s = storm_start.elapsed().as_secs_f64();
    let resync_start = Instant::now();
    let resynced: Vec<anyhow::Result<Resync>> = stream::iter(0..users)
        .map(|m| {
            let (http, token, conv, after) = (
                &http,
                accounts[m].token.clone(),
                group_ids[group_of(m)].clone(),
                cursors[m],
            );
            async move {
                let t0 = Instant::now();
                let mut after = after;
                let mut seqs = HashSet::new();
                let mut pages = 0usize;
                loop {
                    let page: v1::MessagePage = http
                        .call(
                            GET,
                            &format!("/v1/conversations/{conv}/changes?after={after}&limit=500"),
                            Some(&token),
                            None,
                        )
                        .await?;
                    pages += 1;
                    for msg in &page.messages {
                        seqs.insert(msg.server_sequence);
                        after = after.max(msg.change_seq);
                    }
                    if !page.has_more {
                        break;
                    }
                }
                Ok((m, seqs, ms(t0.elapsed()), pages))
            }
        })
        .buffer_unordered(64)
        .collect()
        .await;
    let resync_all_s = resync_start.elapsed().as_secs_f64();
    let mut resync_ms = Vec::new();
    let mut resync_pages = 0usize;
    let mut resync_got: HashMap<usize, HashSet<i64>> = HashMap::new();
    for r in resynced {
        let (m, seqs, took, pages) = r?;
        resync_ms.push(took);
        resync_pages += pages;
        resync_got.insert(m, seqs);
    }
    // One more message per group, now live to everybody.
    let mut post_storm: Vec<(usize, i64, Instant)> = Vec::new();
    for (g, members) in groups.iter().enumerate() {
        let req = v1::SendMessageRequest {
            client_nonce: format!("glp-{run_tag}-{g}"),
            body: format!("after storm {g}"),
            reply_to_id: String::new(),
        };
        let t0 = Instant::now();
        let msg: v1::ChatMessage = http
            .call(
                POST,
                &format!("/v1/conversations/{}/messages", group_ids[g]),
                Some(&accounts[members[1]].token),
                Some(req.encode_to_vec()),
            )
            .await?;
        post_storm.push((g, msg.server_sequence, t0));
    }
    let storm_end = Instant::now();
    let _ = col_stop_tx.send(true);
    let (_ev_rx, after) = col.await?;
    let mut storm_missed = 0usize;
    for m in 0..users {
        let g = group_of(m);
        let got = resync_got.get(&m);
        for &(_, seq) in storm_sent.iter().filter(|(sg, _)| *sg == g) {
            let live_after = after.count.contains_key(&(m, group_ids[g].clone(), seq));
            if !got.is_some_and(|s| s.contains(&seq)) && !live_after {
                storm_missed += 1;
            }
        }
    }
    let mut post_missed = 0usize;
    let mut post_ms = Vec::new();
    for &(g, seq, t0) in &post_storm {
        for &m in &groups[g] {
            match after.first_at.get(&(m, group_ids[g].clone(), seq)) {
                Some(at) => post_ms.push(ms(at.saturating_duration_since(t0))),
                None => post_missed += 1,
            }
        }
    }

    // --- history ------------------------------------------------------------------
    let mut history_gaps = 0usize;
    let mut history_dups = 0usize;
    let mut history_mismatch = 0usize;
    let mut history_total = 0usize;
    let mut accepted_total = 0usize;
    for (g, members) in groups.iter().enumerate() {
        let token = &accounts[members[0]].token;
        let mut by_seq: HashMap<i64, String> = HashMap::new();
        let mut before = 0i64;
        loop {
            let page: v1::MessagePage = http
                .call(
                    GET,
                    &format!(
                        "/v1/conversations/{}/messages?before={before}&limit=100",
                        group_ids[g]
                    ),
                    Some(token),
                    None,
                )
                .await?;
            for msg in &page.messages {
                if by_seq
                    .insert(msg.server_sequence, msg.message_id.clone())
                    .is_some()
                {
                    history_dups += 1;
                }
                before = if before == 0 {
                    msg.server_sequence
                } else {
                    before.min(msg.server_sequence)
                };
            }
            if !page.has_more || page.messages.is_empty() {
                break;
            }
        }
        let n = by_seq.len() as i64;
        history_total += by_seq.len();
        history_gaps += (1..=n).filter(|s| !by_seq.contains_key(s)).count();
        let mut accepted: HashMap<i64, &str> = HashMap::new();
        for s in sends.sent.iter().filter(|s| s.group == g) {
            accepted.insert(s.seq, &s.message_id);
        }
        // Storm and post-storm sends: the sequence is known, the id is not kept.
        let later = storm_sent
            .iter()
            .copied()
            .chain(post_storm.iter().map(|&(g, s, _)| (g, s)));
        for (sg, seq) in later {
            if sg == g {
                accepted.entry(seq).or_insert("");
            }
        }
        accepted_total += accepted.len();
        if accepted.len() != by_seq.len() {
            history_mismatch += accepted.len().abs_diff(by_seq.len());
        }
        for (seq, id) in accepted {
            match by_seq.get(&seq) {
                Some(stored) if id.is_empty() || stored == id => {}
                _ => history_mismatch += 1,
            }
        }
    }

    let _ = stop_tx.send(true);
    let samples = sampler.await?;
    for c in &conns {
        let _ = c.cmd.send(Cmd::Close);
    }
    for c in conns.drain(..) {
        let _ = tokio::time::timeout(Duration::from_secs(5), c.task).await;
    }

    let load_s = (load_end - load_start).as_secs_f64();
    let pass = live_missed == 0
        && live_forbidden == 0
        && foreign == 0
        && live.closed == 0
        && churned.probe_wrong == 0
        && churned.revision_wrong == 0
        && sends.dup_mismatch == 0
        && sends.order_violations == 0
        && storm_missed == 0
        && post_missed == 0
        && history_gaps == 0
        && history_dups == 0
        && history_mismatch == 0;
    let removals: usize = churned.windows.values().map(Vec::len).sum();
    let summary = json!({
        "pass": pass,
        "users": users,
        "groups": p.groups,
        "group_size": p.group_size,
        "load": {
            "seconds": load_s,
            "accepted_sends": sends.sent.len(),
            "sends_per_s": sends.sent.len() as f64 / load_s.max(0.001),
            "send_http_ms": stats(sends.send_ms.clone()),
            "same_nonce_pairs": sends.dup_pairs,
            "same_nonce_mismatch": sends.dup_mismatch,
            "per_sender_order_violations": sends.order_violations,
            "server_cpu_pct_of_one_core": (cpu1 - cpu0) as f64 / 10.0 / load_s.max(0.001),
            "db_pool": pool_summary(&samples, load_start, load_end, pool_max),
        },
        "membership": {
            "removals_and_readds": removals,
            "remove_http_ms": stats(churned.remove_ms.clone()),
            "add_http_ms": stats(churned.add_ms.clone()),
            "removed_member_probes": churned.probes,
            "removed_member_probes_not_refused": churned.probe_wrong,
            "membership_revision_wrong": churned.revision_wrong,
        },
        "live": {
            "missed": live_missed,
            "delivered_while_removed": live_forbidden,
            "boundary_messages": live_either_total,
            "boundary_delivered": live_either_got,
            "duplicate_events": live_dups,
            "foreign_conversation_events": foreign,
            "connections_closed": live.closed,
            "delivery_ms": stats(delivery_ms.clone()),
        },
        "reconnect_storm": {
            "messages_while_offline": storm_sent.len(),
            "reconnect_all_s": reconnect_all_s,
            "reconnect_ms": stats(reconnect_ms.clone()),
            "resync_all_s": resync_all_s,
            "resync_ms": stats(resync_ms.clone()),
            "resync_pages": resync_pages,
            "missed_after_resync": storm_missed,
            "post_storm_missed": post_missed,
            "post_storm_delivery_ms": stats(post_ms.clone()),
            "db_pool": pool_summary(&samples, storm_start, storm_end, pool_max),
        },
        "history": {
            "messages": history_total,
            "accepted": accepted_total,
            "gaps": history_gaps,
            "duplicates": history_dups,
            "mismatch": history_mismatch,
        },
        "server_working_set_mib": {
            "before_load": mem_before.working_set_bytes as f64 / MIB,
            "after_load": mem_load.working_set_bytes as f64 / MIB,
        },
    });
    let raw = json!({
        "send_http_ms": sends.send_ms,
        "remove_http_ms": churned.remove_ms,
        "add_http_ms": churned.add_ms,
        "reconnect_ms": reconnect_ms,
        "resync_ms": resync_ms,
    });
    Ok((summary, raw))
}

async fn connect_all(
    addr: &str,
    accounts: &[Account],
    ev_tx: &mpsc::UnboundedSender<Ev>,
) -> anyhow::Result<(Vec<Conn>, Vec<f64>)> {
    let connected: Vec<(usize, anyhow::Result<(Conn, f64)>)> = stream::iter(0..accounts.len())
        .map(|i| {
            let (token, ev_tx) = (accounts[i].token.clone(), ev_tx.clone());
            async move { (i, connect(addr, i, &token, ev_tx).await) }
        })
        .buffer_unordered(64)
        .collect()
        .await;
    let mut conns: Vec<Option<Conn>> = (0..accounts.len()).map(|_| None).collect();
    let mut took = Vec::new();
    for (i, r) in connected {
        let (c, t) = r.with_context(|| format!("connect user {i}"))?;
        took.push(t);
        conns[i] = Some(c);
    }
    Ok((conns.into_iter().map(Option::unwrap).collect(), took))
}

#[allow(clippy::too_many_arguments)]
async fn sender(
    http: &Http,
    accounts: &[Account],
    group_ids: &[String],
    committed: &[AtomicI64],
    g: usize,
    s: usize,
    n: usize,
    interval_ms: u64,
    run_tag: u64,
) -> anyhow::Result<SendOut> {
    let path = format!("/v1/conversations/{}/messages", group_ids[g]);
    let token = &accounts[s].token;
    let mut out = SendOut::default();
    let mut last_seq = 0i64;
    for k in 0..n {
        let req = v1::SendMessageRequest {
            client_nonce: format!("gl-{run_tag}-{g}-{s}-{k}"),
            body: format!("load {g}-{s}-{k}"),
            reply_to_id: String::new(),
        };
        let body = req.encode_to_vec();
        let started = Instant::now();
        let msg: v1::ChatMessage = if k % 10 == 3 {
            // The same nonce twice at once: one stored message, both get it.
            let (a, b) = tokio::join!(
                http.call::<v1::ChatMessage>(POST, &path, Some(token), Some(body.clone())),
                http.call::<v1::ChatMessage>(POST, &path, Some(token), Some(body)),
            );
            let (a, b) = (a?, b?);
            out.dup_pairs += 1;
            if a.message_id != b.message_id || a.server_sequence != b.server_sequence {
                out.dup_mismatch += 1;
            }
            a
        } else {
            http.call(POST, &path, Some(token), Some(body))
                .await
                .with_context(|| format!("group {g} sender {s} message {k}"))?
        };
        out.send_ms.push(ms(started.elapsed()));
        if msg.server_sequence <= last_seq {
            out.order_violations += 1;
        }
        last_seq = msg.server_sequence;
        committed[g].fetch_max(msg.server_sequence, Ordering::SeqCst);
        out.sent.push(Sent {
            group: g,
            seq: msg.server_sequence,
            message_id: msg.message_id,
            started,
        });
        if interval_ms > 0 {
            tokio::time::sleep(Duration::from_millis(interval_ms)).await;
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
async fn churn(
    http: &Http,
    accounts: &[Account],
    group_ids: &[String],
    committed: &[AtomicI64],
    g: usize,
    creator: usize,
    rotating: &[usize],
    rounds: usize,
    run_tag: u64,
) -> anyhow::Result<ChurnOut> {
    let conv = &group_ids[g];
    let token = &accounts[creator].token;
    let mut out = ChurnOut::default();
    let revision = |c: &v1::Conversation, user: &str| {
        c.members
            .iter()
            .find(|m| m.user.as_ref().is_some_and(|u| u.user_id == user))
            .map(|m| m.membership_revision)
    };
    let mut revisions: HashMap<usize, i64> = HashMap::new();
    for round in 0..rounds {
        let mut open: Vec<(usize, i64, i64)> = Vec::new();
        for &m in rotating {
            let lo = committed[g].load(Ordering::SeqCst);
            let t0 = Instant::now();
            let c: v1::Conversation = http
                .call(
                    DELETE,
                    &format!("/v1/conversations/{conv}/members/{}", accounts[m].user_id),
                    Some(token),
                    None,
                )
                .await
                .with_context(|| format!("remove member {m}"))?;
            out.remove_ms.push(ms(t0.elapsed()));
            if revision(&c, &accounts[m].user_id).is_some() {
                bail!("removed member {m} still listed");
            }
            open.push((m, lo, c.last_sequence));
        }
        // While out: writing and reading must be refused (404: no access at all).
        for &(m, _, _) in &open {
            let mt = &accounts[m].token;
            let req = v1::SendMessageRequest {
                client_nonce: format!("glr-{run_tag}-{g}-{m}-{round}"),
                body: "must be refused".into(),
                reply_to_id: String::new(),
            };
            let (st, _) = http
                .raw(
                    POST,
                    &format!("/v1/conversations/{conv}/messages"),
                    Some(mt),
                    Some(req.encode_to_vec()),
                )
                .await?;
            let (hs, _) = http
                .raw(
                    GET,
                    &format!("/v1/conversations/{conv}/messages"),
                    Some(mt),
                    None,
                )
                .await?;
            out.probes += 2;
            out.probe_wrong += usize::from(st != reqwest::StatusCode::NOT_FOUND)
                + usize::from(hs != reqwest::StatusCode::NOT_FOUND);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        for (m, rem_lo, rem_hi) in open {
            let add_lo = committed[g].load(Ordering::SeqCst);
            let req = v1::AddGroupMemberRequest {
                username: accounts[m].username.clone(),
            };
            let t0 = Instant::now();
            let c: v1::Conversation = http
                .call(
                    POST,
                    &format!("/v1/conversations/{conv}/members"),
                    Some(token),
                    Some(req.encode_to_vec()),
                )
                .await
                .with_context(|| format!("re-add member {m}"))?;
            out.add_ms.push(ms(t0.elapsed()));
            match revision(&c, &accounts[m].user_id) {
                Some(r) => {
                    let prev = revisions.insert(m, r);
                    // A new membership starts at 1; every re-add increments it.
                    if r <= prev.unwrap_or(1) {
                        out.revision_wrong += 1;
                    }
                }
                None => out.revision_wrong += 1,
            }
            out.windows.entry(m).or_default().push(Window {
                rem_lo,
                rem_hi,
                add_lo,
                add_hi: c.last_sequence,
            });
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{Expect, Window, expect};

    fn is(e: Expect, want: &str) -> bool {
        matches!(
            (e, want),
            (Expect::Must, "must") | (Expect::MustNot, "not") | (Expect::Either, "either")
        )
    }

    #[test]
    fn membership_windows_bracket_each_change() {
        // Removed between 10 and 12 (both observed), re-added between 20 and 23.
        let w = [Window {
            rem_lo: 10,
            rem_hi: 12,
            add_lo: 20,
            add_hi: 23,
        }];
        for (s, want) in [
            (1, "must"),
            (10, "must"),
            (11, "either"),
            (12, "either"),
            (13, "not"),
            (20, "not"),
            (21, "either"),
            (23, "either"),
            (24, "must"),
        ] {
            assert!(is(expect(&w, s), want), "seq {s}: want {want}");
        }
        assert!(is(expect(&[], 5), "must"));
    }

    #[test]
    fn overlapping_brackets_never_hide_a_certain_removal() {
        // The re-add was requested before the removal's response arrived, so the
        // brackets overlap: nothing in between is certain either way.
        let w = [Window {
            rem_lo: 5,
            rem_hi: 9,
            add_lo: 8,
            add_hi: 12,
        }];
        assert!(is(expect(&w, 7), "either"));
        assert!(is(expect(&w, 10), "either"));
        assert!(is(expect(&w, 13), "must"));
        let w = [
            Window {
                rem_lo: 1,
                rem_hi: 2,
                add_lo: 5,
                add_hi: 6,
            },
            Window {
                rem_lo: 8,
                rem_hi: 9,
                add_lo: 12,
                add_hi: 13,
            },
        ];
        assert!(is(expect(&w, 4), "not"));
        assert!(is(expect(&w, 7), "must"));
        assert!(is(expect(&w, 11), "not"));
        assert!(is(expect(&w, 14), "must"));
    }
}
