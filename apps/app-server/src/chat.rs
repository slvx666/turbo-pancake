//! Users, blocks, direct conversations, messages and receipts (spec §7, §8, §10;
//! ADR 0013 §2–4, §7). Rights are checked here on every call, never by the UI.
//!
//! Message writes lock the conversation row by bumping its counters first, so writers
//! of one conversation are serialised and `server_sequence` / `change_seq` are gap-free.
//! Events are sent after the commit; their loss is repaired by the change feed.

use std::collections::{HashMap, HashSet};

use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use sqlx::{PgPool, Postgres, Transaction};
use turbo_pancake_protocol::{self as proto, v1};
use uuid::Uuid;
use v1::server_envelope::Body as S;

use crate::AppState;
use crate::api::{ApiError, ApiResult, AuthUser, Db, Proto, ms, parse_id};
use crate::auth::{UserRow, user_by_id};

/// `key=value` pairs of a query string (values here never need percent-decoding:
/// numbers and `[A-Za-z0-9_]` usernames; anything else is invalid input anyway).
fn query_param<'a>(raw: &'a Option<String>, key: &str) -> Option<&'a str> {
    raw.as_deref()?
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

fn query_num<T: std::str::FromStr>(raw: &Option<String>, key: &str) -> ApiResult<Option<T>> {
    query_param(raw, key)
        .map(|v| {
            v.parse()
                .map_err(|_| ApiError::bad_request(format!("{key} must be a number")))
        })
        .transpose()
}

/// `(a blocked b, b blocked a)`.
async fn blocks_between(pool: &PgPool, a: Uuid, b: Uuid) -> sqlx::Result<(bool, bool)> {
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT blocker_user_id FROM blocks
          WHERE (blocker_user_id = $1 AND blocked_user_id = $2)
             OR (blocker_user_id = $2 AND blocked_user_id = $1)",
    )
    .bind(a)
    .bind(b)
    .fetch_all(pool)
    .await?;
    Ok((
        rows.iter().any(|(x,)| *x == a),
        rows.iter().any(|(x,)| *x == b),
    ))
}

/// Presence as `viewer` may see it.
async fn visible_online(
    state: &AppState,
    pool: &PgPool,
    viewer: Uuid,
    user: Uuid,
) -> sqlx::Result<bool> {
    if viewer == user {
        return Ok(true);
    }
    let (x, y) = blocks_between(pool, viewer, user).await?;
    Ok(!x && !y && state.registry.is_online(user))
}

// ---- Users and blocks ----

pub async fn lookup(
    State(state): State<AppState>,
    Db(pool): Db,
    me: AuthUser,
    RawQuery(q): RawQuery,
) -> ApiResult<Proto<v1::User>> {
    if !state.limiters.lookup.check(&me.user_id) {
        return Err(ApiError::rate_limited());
    }
    let name = query_param(&q, "username").unwrap_or("");
    if name.is_empty() || name.len() > proto::MAX_USERNAME_LEN {
        return Err(ApiError::bad_request("username is required"));
    }
    let row: Option<UserRow> = sqlx::query_as(
        "SELECT id, username_display, display_name FROM users
          WHERE username_key = $1 AND status = 'active'",
    )
    .bind(proto::username_key(name))
    .fetch_optional(&pool)
    .await?;
    let row = row.ok_or_else(ApiError::not_found)?;
    let online = visible_online(&state, &pool, me.user_id, row.id).await?;
    Ok(Proto(row.to_proto(online)))
}

pub async fn list_blocks(Db(pool): Db, me: AuthUser) -> ApiResult<Proto<v1::UserList>> {
    let rows: Vec<UserRow> = sqlx::query_as(
        "SELECT u.id, u.username_display, u.display_name
           FROM blocks b JOIN users u ON u.id = b.blocked_user_id
          WHERE b.blocker_user_id = $1 ORDER BY b.created_at",
    )
    .bind(me.user_id)
    .fetch_all(&pool)
    .await?;
    Ok(Proto(v1::UserList {
        users: rows.iter().map(|u| u.to_proto(false)).collect(),
    }))
}

async fn direct_between(pool: &PgPool, a: Uuid, b: Uuid) -> sqlx::Result<Option<Uuid>> {
    let (low, high) = if a < b { (a, b) } else { (b, a) };
    sqlx::query_scalar(
        "SELECT id FROM conversations WHERE kind = 'direct' AND direct_low = $1 AND direct_high = $2",
    )
    .bind(low)
    .bind(high)
    .fetch_optional(pool)
    .await
}

/// After a block change: both sides refetch their direct conversation and see the
/// presence the new state allows.
async fn after_block_change(
    state: &AppState,
    pool: &PgPool,
    me: Uuid,
    other: Uuid,
) -> sqlx::Result<()> {
    if let Some(conv) = direct_between(pool, me, other).await? {
        let body = S::ConversationUpdated(v1::ConversationUpdated {
            conversation_id: conv.to_string(),
        });
        state.registry.send(me, body.clone());
        state.registry.send(other, body);
    }
    for (viewer, user) in [(me, other), (other, me)] {
        let online = visible_online(state, pool, viewer, user).await?;
        state.registry.send(
            viewer,
            S::PresenceChanged(v1::PresenceChanged {
                user_id: user.to_string(),
                online,
            }),
        );
    }
    Ok(())
}

pub async fn block(
    State(state): State<AppState>,
    Db(pool): Db,
    me: AuthUser,
    Path(id): Path<String>,
) -> ApiResult<Proto<v1::Empty>> {
    let other = parse_id(&id)?;
    let mut calls = state.calls.lock().await;
    if other == me.user_id {
        return Err(ApiError::bad_request("cannot block yourself"));
    }
    user_by_id(&pool, other)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let added = sqlx::query(
        "INSERT INTO blocks (blocker_user_id, blocked_user_id) VALUES ($1, $2)
         ON CONFLICT DO NOTHING",
    )
    .bind(me.user_id)
    .bind(other)
    .execute(&pool)
    .await?
    .rows_affected()
        > 0;
    if added {
        tracing::info!(target: "security", event = "blocked", user = %me.user_id, blocked = %other);
        after_block_change(&state, &pool, me.user_id, other).await?;
        if let Some(conv) = direct_between(&pool, me.user_id, other).await? {
            calls.end_conversation(&state, conv, "blocked");
        }
    }
    Ok(Proto(v1::Empty {}))
}

pub async fn unblock(
    State(state): State<AppState>,
    Db(pool): Db,
    me: AuthUser,
    Path(id): Path<String>,
) -> ApiResult<Proto<v1::Empty>> {
    let other = parse_id(&id)?;
    let removed =
        sqlx::query("DELETE FROM blocks WHERE blocker_user_id = $1 AND blocked_user_id = $2")
            .bind(me.user_id)
            .bind(other)
            .execute(&pool)
            .await?
            .rows_affected()
            > 0;
    if removed {
        tracing::info!(target: "security", event = "unblocked", user = %me.user_id, unblocked = %other);
        after_block_change(&state, &pool, me.user_id, other).await?;
    }
    Ok(Proto(v1::Empty {}))
}

// ---- Conversations ----

#[derive(sqlx::FromRow)]
struct ConvRow {
    id: Uuid,
    kind: String,
    title: String,
    created_at: time::OffsetDateTime,
    message_seq: i64,
    change_seq: i64,
}

#[derive(sqlx::FromRow)]
struct MemberRow {
    conversation_id: Uuid,
    user_id: Uuid,
    role: String,
    delivered_sequence: i64,
    read_sequence: i64,
    membership_revision: i64,
    username_display: String,
    display_name: String,
}

/// Conversations of `me` (all, or just `only`) as `me` sees them.
pub(crate) async fn conversations_for(
    state: &AppState,
    pool: &PgPool,
    me: Uuid,
    only: Option<Uuid>,
) -> sqlx::Result<Vec<v1::Conversation>> {
    let convs: Vec<ConvRow> = sqlx::query_as(
        "SELECT c.id, c.kind, c.title, c.created_at, c.message_seq, c.change_seq
           FROM conversations c JOIN conversation_members m ON m.conversation_id = c.id
          WHERE m.user_id = $1 AND m.left_at IS NULL AND ($2::uuid IS NULL OR c.id = $2)
          ORDER BY c.created_at, c.id",
    )
    .bind(me)
    .bind(only)
    .fetch_all(pool)
    .await?;
    if convs.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = convs.iter().map(|c| c.id).collect();
    let members: Vec<MemberRow> = sqlx::query_as(
        "SELECT m.conversation_id, m.user_id, m.role, m.delivered_sequence, m.read_sequence, m.membership_revision,
                u.username_display, u.display_name
           FROM conversation_members m JOIN users u ON u.id = m.user_id
          WHERE m.conversation_id = ANY($1) AND m.left_at IS NULL
          ORDER BY m.joined_at, m.user_id",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?;
    let blocks: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT blocker_user_id, blocked_user_id FROM blocks
          WHERE blocker_user_id = $1 OR blocked_user_id = $1",
    )
    .bind(me)
    .fetch_all(pool)
    .await?;
    let blocked_by_me: HashSet<Uuid> = blocks
        .iter()
        .filter(|(a, _)| *a == me)
        .map(|(_, b)| *b)
        .collect();
    let in_block: HashSet<Uuid> = blocks
        .iter()
        .map(|(a, b)| if *a == me { *b } else { *a })
        .collect();
    let mut by_conv: HashMap<Uuid, Vec<&MemberRow>> = HashMap::new();
    for m in &members {
        by_conv.entry(m.conversation_id).or_default().push(m);
    }
    Ok(convs
        .iter()
        .map(|c| {
            let ms_ = by_conv.remove(&c.id).unwrap_or_default();
            let direct = c.kind == "direct";
            let other = ms_.iter().map(|m| m.user_id).find(|u| *u != me);
            let (blocked, can_send) = match (direct, other) {
                (true, Some(o)) => (blocked_by_me.contains(&o), !in_block.contains(&o)),
                (true, None) => (false, false),
                (false, _) => (false, true),
            };
            v1::Conversation {
                conversation_id: c.id.to_string(),
                kind: if direct {
                    v1::ConversationKind::Direct
                } else {
                    v1::ConversationKind::Group
                } as i32,
                title: c.title.clone(),
                members: ms_
                    .iter()
                    .map(|m| v1::Member {
                        user: Some(v1::User {
                            user_id: m.user_id.to_string(),
                            username: m.username_display.clone(),
                            display_name: m.display_name.clone(),
                            online: m.user_id == me
                                || (!in_block.contains(&m.user_id)
                                    && state.registry.is_online(m.user_id)),
                        }),
                        role: if m.role == "creator" {
                            v1::MemberRole::Creator
                        } else {
                            v1::MemberRole::Member
                        } as i32,
                        delivered_sequence: m.delivered_sequence,
                        read_sequence: m.read_sequence,
                        membership_revision: m.membership_revision,
                    })
                    .collect(),
                last_sequence: c.message_seq,
                change_seq: c.change_seq,
                created_at_ms: ms(c.created_at),
                blocked_by_me: blocked,
                can_send,
            }
        })
        .collect())
}

pub async fn list_conversations(
    State(state): State<AppState>,
    Db(pool): Db,
    me: AuthUser,
) -> ApiResult<Proto<v1::ConversationList>> {
    let conversations = conversations_for(&state, &pool, me.user_id, None).await?;
    Ok(Proto(v1::ConversationList { conversations }))
}

pub async fn create_direct(
    State(state): State<AppState>,
    Db(pool): Db,
    me: AuthUser,
    Proto(req): Proto<v1::CreateDirectRequest>,
) -> ApiResult<Proto<v1::Conversation>> {
    proto::validate_id(&req.user_id)?;
    let other = Uuid::parse_str(&req.user_id).map_err(|_| ApiError::bad_request("malformed id"))?;
    if other == me.user_id {
        return Err(ApiError::bad_request(
            "cannot start a conversation with yourself",
        ));
    }
    if !state.limiters.create_conversation.check(&me.user_id) {
        return Err(ApiError::rate_limited());
    }
    user_by_id(&pool, other)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let (low, high) = if me.user_id < other {
        (me.user_id, other)
    } else {
        (other, me.user_id)
    };
    let mut tx = pool.begin().await?;
    let created: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO conversations (id, kind, created_by, direct_low, direct_high)
         VALUES ($1, 'direct', $2, $3, $4)
         ON CONFLICT (direct_low, direct_high) WHERE kind = 'direct' DO NOTHING
         RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(me.user_id)
    .bind(low)
    .bind(high)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(id) = created {
        sqlx::query(
            "INSERT INTO conversation_members (conversation_id, user_id, role)
             VALUES ($1, $2, 'member'), ($1, $3, 'member')",
        )
        .bind(id)
        .bind(low)
        .bind(high)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    let id = match created {
        Some(id) => {
            let body = S::ConversationUpdated(v1::ConversationUpdated {
                conversation_id: id.to_string(),
            });
            state.registry.send(me.user_id, body.clone());
            state.registry.send(other, body);
            // Both now see each other's presence.
            crate::events::announce_presence(pool.clone(), state.registry.clone(), me.user_id);
            crate::events::announce_presence(pool.clone(), state.registry.clone(), other);
            id
        }
        None => direct_between(&pool, low, high)
            .await?
            .ok_or_else(ApiError::internal)?,
    };
    let conv = conversations_for(&state, &pool, me.user_id, Some(id))
        .await?
        .pop()
        .ok_or_else(ApiError::internal)?;
    Ok(Proto(conv))
}

/// Display name of `user` changed: every conversation partner refetches.
pub async fn notify_contacts_changed(
    pool: &PgPool,
    state: &AppState,
    user: Uuid,
) -> sqlx::Result<()> {
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT m2.conversation_id, m2.user_id
           FROM conversation_members m1
           JOIN conversation_members m2 ON m2.conversation_id = m1.conversation_id
          WHERE m1.user_id = $1 AND m1.left_at IS NULL AND m2.left_at IS NULL",
    )
    .bind(user)
    .fetch_all(pool)
    .await?;
    for (conv, member) in rows {
        state.registry.send(
            member,
            S::ConversationUpdated(v1::ConversationUpdated {
                conversation_id: conv.to_string(),
            }),
        );
    }
    Ok(())
}

// ---- Messages ----

struct Access {
    /// Direct conversation: sending is allowed (no block either way).
    can_send: bool,
    members: Vec<Uuid>,
}

/// `me` is a member of `conv` (else 404, so non-members learn nothing).
async fn access(pool: &PgPool, conv: Uuid, me: Uuid) -> ApiResult<Access> {
    access_on(&mut *pool.acquire().await?, conv, me).await
}

async fn access_on(conn: &mut sqlx::PgConnection, conv: Uuid, me: Uuid) -> ApiResult<Access> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT m.user_id, c.kind FROM conversation_members m
           JOIN conversations c ON c.id = m.conversation_id
          WHERE m.conversation_id = $1 AND m.left_at IS NULL",
    )
    .bind(conv)
    .fetch_all(&mut *conn)
    .await?;
    let members: Vec<Uuid> = rows.iter().map(|(u, _)| *u).collect();
    if !members.contains(&me) {
        return Err(ApiError::not_found());
    }
    let direct = rows.first().is_some_and(|(_, k)| k == "direct");
    let can_send = if direct {
        match members.iter().find(|u| **u != me) {
            Some(other) => {
                let blocked: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM blocks
                     WHERE (blocker_user_id = $1 AND blocked_user_id = $2)
                        OR (blocker_user_id = $2 AND blocked_user_id = $1))",
                )
                .bind(me)
                .bind(other)
                .fetch_one(&mut *conn)
                .await?;
                !blocked
            }
            None => false,
        }
    } else {
        true
    };
    Ok(Access { can_send, members })
}

const BLOCKED: &str = "you cannot message this user";

#[derive(sqlx::FromRow)]
struct MsgRow {
    id: Uuid,
    conversation_id: Uuid,
    server_sequence: i64,
    change_seq: i64,
    author_id: Uuid,
    client_nonce: String,
    body: Option<String>,
    reply_to_id: Option<Uuid>,
    created_at: time::OffsetDateTime,
    edited_at: Option<time::OffsetDateTime>,
    deleted_at: Option<time::OffsetDateTime>,
}

const MSG_COLS: &str = "id, conversation_id, server_sequence, change_seq, author_id, client_nonce, \
                        body, reply_to_id, created_at, edited_at, deleted_at";

impl MsgRow {
    /// The nonce is the author's own idempotency key: others do not get it.
    fn to_proto(&self, viewer: Uuid) -> v1::ChatMessage {
        v1::ChatMessage {
            message_id: self.id.to_string(),
            conversation_id: self.conversation_id.to_string(),
            server_sequence: self.server_sequence,
            change_seq: self.change_seq,
            author_id: self.author_id.to_string(),
            client_nonce: if viewer == self.author_id {
                self.client_nonce.clone()
            } else {
                String::new()
            },
            body: self.body.clone().unwrap_or_default(),
            reply_to_id: self.reply_to_id.map(|r| r.to_string()).unwrap_or_default(),
            created_at_ms: ms(self.created_at),
            edited_at_ms: self.edited_at.map(ms).unwrap_or(0),
            deleted: self.deleted_at.is_some(),
        }
    }
}

fn fan_out(state: &AppState, members: &[Uuid], row: &MsgRow, kind: fn(v1::MessageEvent) -> S) {
    for m in members {
        state.registry.send(
            *m,
            kind(v1::MessageEvent {
                message: Some(row.to_proto(*m)),
            }),
        );
    }
}

pub async fn history(
    Db(pool): Db,
    me: AuthUser,
    Path(conv): Path<String>,
    RawQuery(q): RawQuery,
) -> ApiResult<Proto<v1::MessagePage>> {
    let conv = parse_id(&conv)?;
    access(&pool, conv, me.user_id).await?;
    let before = query_num::<i64>(&q, "before")?
        .filter(|b| *b > 0)
        .unwrap_or(i64::MAX);
    let limit = query_num::<u32>(&q, "limit")?
        .unwrap_or(50)
        .clamp(1, proto::MAX_HISTORY_PAGE);
    let upto = committed_change_seq(&pool, conv).await?;
    let mut rows: Vec<MsgRow> = sqlx::query_as(&format!(
        "SELECT {MSG_COLS} FROM messages
          WHERE conversation_id = $1 AND server_sequence < $2 AND change_seq <= $4
          ORDER BY server_sequence DESC LIMIT $3"
    ))
    .bind(conv)
    .bind(before)
    .bind(i64::from(limit) + 1)
    .bind(upto)
    .fetch_all(&pool)
    .await?;
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    Ok(page(me.user_id, rows, has_more, upto))
}

pub async fn changes(
    Db(pool): Db,
    me: AuthUser,
    Path(conv): Path<String>,
    RawQuery(q): RawQuery,
) -> ApiResult<Proto<v1::MessagePage>> {
    let conv = parse_id(&conv)?;
    access(&pool, conv, me.user_id).await?;
    let after = query_num::<i64>(&q, "after")?.unwrap_or(0).max(0);
    let limit = query_num::<u32>(&q, "limit")?
        .unwrap_or(200)
        .clamp(1, proto::MAX_CHANGES_PAGE);
    let upto = committed_change_seq(&pool, conv).await?;
    let mut rows: Vec<MsgRow> = sqlx::query_as(&format!(
        "SELECT {MSG_COLS} FROM messages
          WHERE conversation_id = $1 AND change_seq > $2 AND change_seq <= $4
          ORDER BY change_seq LIMIT $3"
    ))
    .bind(conv)
    .bind(after)
    .bind(i64::from(limit) + 1)
    .bind(upto)
    .fetch_all(&pool)
    .await?;
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    Ok(page(me.user_id, rows, has_more, upto))
}

/// The conversation's committed change_seq, read *before* the page. Writers of one
/// conversation commit in change_seq order (row lock), so every change up to this value
/// is visible and the page is a gap-free prefix; a change committed meanwhile has a
/// larger value and comes with the next call or its event. (Reading the counter after
/// the page let a client's cursor jump over a message committed in between.)
async fn committed_change_seq(pool: &PgPool, conv: Uuid) -> sqlx::Result<i64> {
    sqlx::query_scalar("SELECT change_seq FROM conversations WHERE id = $1")
        .bind(conv)
        .fetch_one(pool)
        .await
}

fn page(me: Uuid, rows: Vec<MsgRow>, has_more: bool, change_seq: i64) -> Proto<v1::MessagePage> {
    Proto(v1::MessagePage {
        messages: rows.iter().map(|r| r.to_proto(me)).collect(),
        has_more,
        change_seq,
    })
}

/// Locks the conversation row and takes the next change_seq (and message_seq when
/// `new_message`). Rolled back with the transaction if the write fails.
async fn bump(
    tx: &mut Transaction<'_, Postgres>,
    conv: Uuid,
    new_message: bool,
) -> sqlx::Result<(i64, i64)> {
    sqlx::query_as(
        "UPDATE conversations
            SET change_seq = change_seq + 1,
                message_seq = message_seq + CASE WHEN $2 THEN 1 ELSE 0 END
          WHERE id = $1 RETURNING message_seq, change_seq",
    )
    .bind(conv)
    .bind(new_message)
    .fetch_one(&mut **tx)
    .await
}

pub async fn send_message(
    State(state): State<AppState>,
    Db(pool): Db,
    me: AuthUser,
    Path(conv): Path<String>,
    Proto(req): Proto<v1::SendMessageRequest>,
) -> ApiResult<(StatusCode, Proto<v1::ChatMessage>)> {
    let conv = parse_id(&conv)?;
    proto::validate_nonce(&req.client_nonce)?;
    proto::validate_message_body(&req.body)?;
    let reply_to = if req.reply_to_id.is_empty() {
        None
    } else {
        proto::validate_id(&req.reply_to_id)?;
        Some(Uuid::parse_str(&req.reply_to_id).map_err(|_| ApiError::bad_request("malformed id"))?)
    };
    if !state.limiters.message_writes.check(&me.user_id) {
        return Err(ApiError::rate_limited());
    }
    access(&pool, conv, me.user_id).await?;
    let mut tx = pool.begin().await?;
    let (seq, change) = bump(&mut tx, conv, true).await?;
    let acc = access_on(&mut tx, conv, me.user_id).await?;
    // After the lock: a concurrent retry of the same nonce has committed by now.
    let existing: Option<MsgRow> = sqlx::query_as(&format!(
        "SELECT {MSG_COLS} FROM messages WHERE author_id = $1 AND client_nonce = $2"
    ))
    .bind(me.user_id)
    .bind(&req.client_nonce)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(row) = existing {
        tx.rollback().await?;
        if row.conversation_id != conv {
            return Err(ApiError::bad_request("client nonce already used"));
        }
        return Ok((StatusCode::OK, Proto(row.to_proto(me.user_id))));
    }
    if !acc.can_send {
        return Err(ApiError::forbidden(BLOCKED));
    }
    if let Some(r) = reply_to {
        let found: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM messages WHERE id = $1 AND conversation_id = $2")
                .bind(r)
                .bind(conv)
                .fetch_optional(&mut *tx)
                .await?;
        if found.is_none() {
            return Err(ApiError::bad_request(
                "replied message is not in this conversation",
            ));
        }
    }
    let row: MsgRow = sqlx::query_as(&format!(
        "INSERT INTO messages (id, conversation_id, server_sequence, change_seq, author_id,
                               client_nonce, body, reply_to_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING {MSG_COLS}"
    ))
    .bind(Uuid::new_v4())
    .bind(conv)
    .bind(seq)
    .bind(change)
    .bind(me.user_id)
    .bind(&req.client_nonce)
    .bind(&req.body)
    .bind(reply_to)
    .fetch_one(&mut *tx)
    .await?;
    // The author has seen their own message on the sending device.
    sqlx::query(
        "UPDATE conversation_members
            SET delivered_sequence = GREATEST(delivered_sequence, $3),
                read_sequence = GREATEST(read_sequence, $3)
          WHERE conversation_id = $1 AND user_id = $2",
    )
    .bind(conv)
    .bind(me.user_id)
    .bind(seq)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    fan_out(&state, &acc.members, &row, S::MessageCreated);
    Ok((StatusCode::CREATED, Proto(row.to_proto(me.user_id))))
}

/// Shared part of edit and delete: the message, locked, written by `me`.
async fn own_message(
    tx: &mut Transaction<'_, Postgres>,
    conv: Uuid,
    msg: Uuid,
    me: Uuid,
) -> ApiResult<MsgRow> {
    let row: Option<MsgRow> = sqlx::query_as(&format!(
        "SELECT {MSG_COLS} FROM messages WHERE id = $1 AND conversation_id = $2 FOR UPDATE"
    ))
    .bind(msg)
    .bind(conv)
    .fetch_optional(&mut **tx)
    .await?;
    let row = row.ok_or_else(ApiError::not_found)?;
    if row.author_id != me {
        return Err(ApiError::forbidden("only the author can change a message"));
    }
    Ok(row)
}

pub async fn edit_message(
    State(state): State<AppState>,
    Db(pool): Db,
    me: AuthUser,
    Path((conv, msg)): Path<(String, String)>,
    Proto(req): Proto<v1::EditMessageRequest>,
) -> ApiResult<Proto<v1::ChatMessage>> {
    let (conv, msg) = (parse_id(&conv)?, parse_id(&msg)?);
    proto::validate_message_body(&req.body)?;
    if !state.limiters.message_writes.check(&me.user_id) {
        return Err(ApiError::rate_limited());
    }
    access(&pool, conv, me.user_id).await?;
    let mut tx = pool.begin().await?;
    let (_, change) = bump(&mut tx, conv, false).await?;
    let acc = access_on(&mut tx, conv, me.user_id).await?;
    if !acc.can_send {
        return Err(ApiError::forbidden(BLOCKED));
    }
    let row = own_message(&mut tx, conv, msg, me.user_id).await?;
    if row.deleted_at.is_some() {
        return Err(ApiError::not_found());
    }
    let row: MsgRow = sqlx::query_as(&format!(
        "UPDATE messages SET body = $2, edited_at = now(), change_seq = $3
          WHERE id = $1 RETURNING {MSG_COLS}"
    ))
    .bind(msg)
    .bind(&req.body)
    .bind(change)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    fan_out(&state, &acc.members, &row, S::MessageEdited);
    Ok(Proto(row.to_proto(me.user_id)))
}

/// Delete for everyone: the body is erased in the database, the tombstone keeps the id
/// (replies) and goes through the change feed to every cache.
pub async fn delete_message(
    State(state): State<AppState>,
    Db(pool): Db,
    me: AuthUser,
    Path((conv, msg)): Path<(String, String)>,
) -> ApiResult<Proto<v1::ChatMessage>> {
    let (conv, msg) = (parse_id(&conv)?, parse_id(&msg)?);
    if !state.limiters.message_writes.check(&me.user_id) {
        return Err(ApiError::rate_limited());
    }
    access(&pool, conv, me.user_id).await?;
    let mut tx = pool.begin().await?;
    let (_, change) = bump(&mut tx, conv, false).await?;
    let acc = access_on(&mut tx, conv, me.user_id).await?;
    let row = own_message(&mut tx, conv, msg, me.user_id).await?;
    if row.deleted_at.is_some() {
        tx.rollback().await?;
        return Ok(Proto(row.to_proto(me.user_id)));
    }
    let row: MsgRow = sqlx::query_as(&format!(
        "UPDATE messages SET body = NULL, deleted_at = now(), change_seq = $2
          WHERE id = $1 RETURNING {MSG_COLS}"
    ))
    .bind(msg)
    .bind(change)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    fan_out(&state, &acc.members, &row, S::MessageDeleted);
    Ok(Proto(row.to_proto(me.user_id)))
}

pub async fn mark_read(
    State(state): State<AppState>,
    Db(pool): Db,
    me: AuthUser,
    Path(conv): Path<String>,
    Proto(req): Proto<v1::MarkReadRequest>,
) -> ApiResult<Proto<v1::Receipt>> {
    let conv = parse_id(&conv)?;
    access(&pool, conv, me.user_id).await?;
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM conversations WHERE id = $1 FOR UPDATE")
        .bind(conv)
        .execute(&mut *tx)
        .await?;
    let acc = access_on(&mut tx, conv, me.user_id).await?;
    // Clamped to the newest message; values only go up; read implies delivered.
    let row: (i64, i64, bool) = sqlx::query_as(
        "WITH old AS (
             SELECT delivered_sequence AS d, read_sequence AS r FROM conversation_members
              WHERE conversation_id = $1 AND user_id = $2),
         lim AS (SELECT message_seq AS s FROM conversations WHERE id = $1)
         UPDATE conversation_members m
            SET read_sequence = GREATEST(m.read_sequence, LEAST($4, lim.s)),
                delivered_sequence = GREATEST(m.delivered_sequence, LEAST(GREATEST($3, $4), lim.s))
           FROM lim, old
          WHERE m.conversation_id = $1 AND m.user_id = $2
         RETURNING m.delivered_sequence, m.read_sequence,
                   (m.delivered_sequence <> old.d OR m.read_sequence <> old.r)",
    )
    .bind(conv)
    .bind(me.user_id)
    .bind(req.delivered_sequence)
    .bind(req.read_sequence)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    let receipt = v1::Receipt {
        conversation_id: conv.to_string(),
        user_id: me.user_id.to_string(),
        delivered_sequence: row.0,
        read_sequence: row.1,
    };
    if row.2 {
        for m in &acc.members {
            state.registry.send(*m, S::ReadUpdated(receipt.clone()));
        }
    }
    Ok(Proto(receipt))
}
