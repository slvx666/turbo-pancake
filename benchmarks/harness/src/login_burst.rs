//! `login-burst` scenario (S8 step 4, issue 45): `--logins` parallel logins of one
//! account against the release server; server private bytes are sampled every 10 ms.
//! With the hashing bound (`--hash-concurrency`, `--hash-queue` passed to the server)
//! the peak must stay near concurrency × 19 MiB above the idle server, the overflow is
//! answered `503` + `Retry-After`, and no correct password is ever answered `401`.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures_util::{StreamExt, stream};
use prost::Message as _;
use serde_json::json;
use turbo_pancake_protocol::v1;

use crate::messenger::{PASSWORD, ms, ready, start_server};
use crate::{MIB, host_info, mem, stats, with_child_diag};

pub struct Params {
    pub logins: usize,
    pub hash_concurrency: usize,
    pub hash_queue: usize,
    pub database_url: Option<String>,
}

pub async fn login_burst(server_exe: &Path, p: Params) -> anyhow::Result<serde_json::Value> {
    let extra = vec![
        "--no-media-udp".to_owned(),
        "--hash-concurrency".into(),
        p.hash_concurrency.to_string(),
        "--hash-queue".into(),
        p.hash_queue.to_string(),
    ];
    let (mut child, rx, tail, db_kind) =
        start_server(server_exe, p.database_url.as_deref(), &extra)?;
    let pid = child.id();
    let result = async {
        let (_, http) = ready(&rx, p.logins).await?;
        let tag = crate::unix_now() % 100_000;
        let username = format!("lb{tag}");
        let reg = v1::RegisterRequest {
            username: username.clone(),
            password: PASSWORD.into(),
            display_name: String::new(),
            device_name: "turbo-pancake-bench".into(),
        };
        let _: v1::AuthResponse = http
            .call(
                reqwest::Method::POST,
                "/v1/auth/register",
                None,
                Some(reg.encode_to_vec()),
            )
            .await?;
        // Let the allocator settle after the first hash.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let idle = mem::sample(pid)?;
        let stop = Arc::new(AtomicBool::new(false));
        let sampler = {
            let stop = stop.clone();
            tokio::spawn(async move {
                let mut peak = (0u64, 0u64);
                while !stop.load(Ordering::Relaxed) {
                    if let Ok(m) = mem::sample(pid) {
                        peak.0 = peak.0.max(m.working_set_bytes);
                        peak.1 = peak.1.max(m.private_bytes.unwrap_or(0));
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                peak
            })
        };
        let body = v1::LoginRequest {
            username,
            password: PASSWORD.into(),
            device_name: "turbo-pancake-bench".into(),
        }
        .encode_to_vec();
        let t = Instant::now();
        let results: Vec<anyhow::Result<(u16, f64)>> = stream::iter(0..p.logins)
            .map(|_| {
                let (http, body) = (&http, body.clone());
                async move {
                    let t0 = Instant::now();
                    let (status, _) = http
                        .raw(reqwest::Method::POST, "/v1/auth/login", None, Some(body))
                        .await?;
                    Ok((status.as_u16(), ms(t0.elapsed())))
                }
            })
            .buffer_unordered(p.logins)
            .collect()
            .await;
        let wall = t.elapsed().as_secs_f64();
        stop.store(true, Ordering::Relaxed);
        let (peak_ws, peak_private) = sampler.await?;
        let (mut ok, mut busy, mut other) = (0usize, 0usize, Vec::new());
        let mut ok_ms = Vec::new();
        for r in results {
            match r? {
                (200, took) => {
                    ok += 1;
                    ok_ms.push(took);
                }
                (503, _) => busy += 1,
                (s, _) => other.push(s),
            }
        }
        let (_, st) = http
            .raw(reqwest::Method::GET, "/v1/server-stats", None, None)
            .await?;
        let st: serde_json::Value = serde_json::from_slice(&st)?;
        let idle_private = idle.private_bytes.unwrap_or(0);
        let pass = other.is_empty()
            && ok > 0
            && st["password_hashing"]["peak_running"].as_u64() <= Some(p.hash_concurrency as u64);
        anyhow::Ok(json!({
            "pass": pass,
            "logins": p.logins,
            "ok": ok,
            "refused_503": busy,
            "other_status": other,
            "wall_s": wall,
            "ok_latency_ms": stats(ok_ms),
            "password_hashing": st["password_hashing"],
            "server_private_mib": {
                "idle": idle_private as f64 / MIB,
                "peak": peak_private as f64 / MIB,
                "peak_above_idle": peak_private.saturating_sub(idle_private) as f64 / MIB,
            },
            "server_working_set_mib": {
                "idle": idle.working_set_bytes as f64 / MIB,
                "peak": peak_ws as f64 / MIB,
            },
        }))
    }
    .await
    .map_err(|e| with_child_diag(e, &mut child, &tail));
    let _ = child.kill();
    let _ = child.wait();
    let summary = result?;
    Ok(json!({
        "scenario": "login-burst",
        "label": format!("{}x-c{}-q{}", p.logins, p.hash_concurrency, p.hash_queue),
        "host": host_info(),
        "params": {
            "logins": p.logins, "hash_concurrency": p.hash_concurrency,
            "hash_queue": p.hash_queue, "database": db_kind,
        },
        "summary": summary,
    }))
}
