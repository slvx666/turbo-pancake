//! Authenticated conversation calls for the media benches (S8 step 3, ADR 0015).
//!
//! With `--accounts` the `sfu` / `sfu-screen` members are real accounts instead of
//! anonymous room peers: accounts through the register API, one group with all of them,
//! the first member starts the group call (`POST /v1/calls`), the others join it
//! (`POST /v1/calls/{id}/join`), and each member's signed-in WebSocket attaches its media
//! with `JoinCallMedia` (participation lease). The server then puts the connection into
//! the call's media room; offers, shares and subscriptions name that room.

use std::time::Instant;

use anyhow::{Context, bail, ensure};
use futures_util::{StreamExt, stream};
use prost::Message as _;
use serde_json::json;
use turbo_pancake_protocol::v1::{self, client_envelope::Body as C, server_envelope::Body as S};

use crate::messenger::{Http, PASSWORD, ms};
use crate::{Ws, stats, ws_recv, ws_send};

/// How a member enters the media room.
#[derive(Clone)]
pub enum Join {
    /// Anonymous peer in a named room (the S1–S4 benches).
    Room { name: String, room: String },
    /// Signed-in account attached to a conversation call.
    Call {
        token: String,
        call_id: String,
        participation_id: String,
    },
}

/// Registers `n` accounts, puts them into one group and joins all of them to its call.
/// Returns one `Join::Call` per member and setup timings for the report.
pub async fn setup(
    http: &Http,
    n: usize,
    concurrency: usize,
) -> anyhow::Result<(Vec<Join>, serde_json::Value)> {
    ensure!(
        (2..=100).contains(&n),
        "accounts mode: 2..=100 members (one group)"
    );
    let tag = crate::unix_now() % 100_000;
    let t = Instant::now();
    let registered: Vec<(usize, anyhow::Result<v1::AuthResponse>)> = stream::iter(0..n)
        .map(|i| async move {
            let req = v1::RegisterRequest {
                username: format!("mc{tag}_{i}"),
                password: PASSWORD.into(),
                display_name: format!("Media {i}"),
                device_name: "turbo-pancake-bench".into(),
            };
            let r = http
                .call(
                    reqwest::Method::POST,
                    "/v1/auth/register",
                    None,
                    Some(req.encode_to_vec()),
                )
                .await;
            (i, r)
        })
        .buffer_unordered(concurrency.max(1))
        .collect()
        .await;
    let register_s = t.elapsed().as_secs_f64();
    let mut accounts: Vec<Option<(String, String)>> = vec![None; n];
    for (i, r) in registered {
        let a = r.with_context(|| format!("register member {i}"))?;
        let user = a.user.context("AuthResponse without user")?;
        accounts[i] = Some((user.username, a.token));
    }
    let accounts: Vec<(String, String)> = accounts.into_iter().map(Option::unwrap).collect();
    let group = v1::CreateGroupRequest {
        title: "Media load".into(),
        usernames: accounts[1..].iter().map(|(u, _)| u.clone()).collect(),
    };
    let conv: v1::Conversation = http
        .call(
            reqwest::Method::POST,
            "/v1/conversations/group",
            Some(&accounts[0].1),
            Some(group.encode_to_vec()),
        )
        .await
        .context("create group")?;
    ensure!(
        conv.members.len() == n,
        "group has {} members",
        conv.members.len()
    );

    let t0 = Instant::now();
    let first: v1::CallParticipation = http
        .call(
            reqwest::Method::POST,
            "/v1/calls",
            Some(&accounts[0].1),
            Some(
                v1::CreateCallRequest {
                    conversation_id: conv.conversation_id.clone(),
                }
                .encode_to_vec(),
            ),
        )
        .await
        .context("create call")?;
    let create_ms = ms(t0.elapsed());
    let call_id = first.call.context("no call")?.call_id;
    let joined: Vec<(usize, anyhow::Result<(v1::CallParticipation, f64)>)> = stream::iter(1..n)
        .map(|i| {
            let (token, path) = (&accounts[i].1, format!("/v1/calls/{call_id}/join"));
            async move {
                let t0 = Instant::now();
                let r = http
                    .call(reqwest::Method::POST, &path, Some(token), None)
                    .await
                    .map(|p| (p, ms(t0.elapsed())));
                (i, r)
            }
        })
        .buffer_unordered(concurrency.max(1))
        .collect()
        .await;
    let mut joins: Vec<Option<Join>> = vec![None; n];
    joins[0] = Some(Join::Call {
        token: accounts[0].1.clone(),
        call_id: call_id.clone(),
        participation_id: first.participation_id,
    });
    let mut join_ms = Vec::new();
    for (i, r) in joined {
        let (p, took) = r.with_context(|| format!("member {i} joins the call"))?;
        join_ms.push(took);
        joins[i] = Some(Join::Call {
            token: accounts[i].1.clone(),
            call_id: call_id.clone(),
            participation_id: p.participation_id,
        });
    }
    let timings = json!({
        "accounts": n,
        "register_all_s": register_s,
        "call_create_ms": create_ms,
        "call_join_http_ms": stats(join_ms),
    });
    Ok((joins.into_iter().map(Option::unwrap).collect(), timings))
}

/// Opens the member's signaling connection and enters its media room. Returns the
/// socket, this connection's peer id and the room the media messages must name.
pub async fn enter(addr: &str, join: &Join) -> anyhow::Result<(Ws, String, String)> {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/v1/signal")).await?;
    let (name, token) = match join {
        Join::Room { name, .. } => (name.clone(), String::new()),
        Join::Call { token, .. } => (String::new(), token.clone()),
    };
    ws_send(
        &mut ws,
        1,
        C::Hello(v1::Hello {
            client_version: "turbo-pancake-bench".into(),
            display_name: name,
            auth_token: token,
        }),
    )
    .await?;
    let peer_id = loop {
        match ws_recv(&mut ws).await? {
            S::Welcome(w) => break w.peer_id,
            S::Error(e) => bail!("hello refused: {}", e.message),
            _ => {}
        }
    };
    match join {
        Join::Room { room, .. } => {
            ws_send(
                &mut ws,
                2,
                C::JoinRoom(v1::JoinRoom {
                    room_id: room.clone(),
                }),
            )
            .await?;
            Ok((ws, peer_id, room.clone()))
        }
        Join::Call {
            call_id,
            participation_id,
            ..
        } => {
            ws_send(
                &mut ws,
                2,
                C::JoinCallMedia(v1::JoinCallMedia {
                    call_id: call_id.clone(),
                    participation_id: participation_id.clone(),
                }),
            )
            .await?;
            loop {
                match ws_recv(&mut ws).await? {
                    S::RoomJoined(j) => break Ok((ws, peer_id, j.room_id)),
                    S::Error(e) => bail!("JoinCallMedia refused: {}", e.message),
                    _ => {}
                }
            }
        }
    }
}
