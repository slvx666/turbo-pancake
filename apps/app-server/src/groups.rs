//! Group membership is serialized with message writes on the conversation row.
//! Snapshots are authoritative; events invalidate them, reconnect reloads the list.

use std::collections::HashSet;

use axum::extract::{Path, State};
use sqlx::{Postgres, Transaction};
use turbo_pancake_protocol::{self as proto, v1};
use uuid::Uuid;

use crate::AppState;
use crate::api::{ApiError, ApiResult, AuthUser, Db, Proto, parse_id};

async fn resolve(tx: &mut Transaction<'_, Postgres>, username: &str) -> ApiResult<Uuid> {
    proto::validate_username(username)?;
    sqlx::query_scalar("SELECT id FROM users WHERE username_key = $1 AND status = 'active'")
        .bind(proto::username_key(username))
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(ApiError::not_found)
}

/// Acquired before checking rights/capacity, and held until mutation commits.
async fn creator(tx: &mut Transaction<'_, Postgres>, conv: Uuid, me: Uuid) -> ApiResult<()> {
    let row: Option<(String, Uuid)> =
        sqlx::query_as("SELECT kind, created_by FROM conversations WHERE id = $1 FOR UPDATE")
            .bind(conv)
            .fetch_optional(&mut **tx)
            .await?;
    let Some((kind, owner)) = row else {
        return Err(ApiError::not_found());
    };
    let member: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM conversation_members
         WHERE conversation_id = $1 AND user_id = $2 AND left_at IS NULL)",
    )
    .bind(conv)
    .bind(me)
    .fetch_one(&mut **tx)
    .await?;
    if !member {
        return Err(ApiError::not_found());
    }
    if kind != "group" || owner != me {
        return Err(ApiError::forbidden("only the group creator can manage it"));
    }
    Ok(())
}

async fn members(tx: &mut Transaction<'_, Postgres>, conv: Uuid) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar(
        "SELECT user_id FROM conversation_members WHERE conversation_id = $1 AND left_at IS NULL",
    )
    .bind(conv)
    .fetch_all(&mut **tx)
    .await
}

async fn finish(
    state: &AppState,
    pool: &sqlx::PgPool,
    me: Uuid,
    conv: Uuid,
    recipients: &[Uuid],
) -> ApiResult<Proto<v1::Conversation>> {
    for user in recipients {
        state.registry.send(
            *user,
            v1::server_envelope::Body::ConversationUpdated(v1::ConversationUpdated {
                conversation_id: conv.to_string(),
            }),
        );
    }
    let snapshot = crate::chat::conversations_for(state, pool, me, Some(conv))
        .await?
        .pop()
        .ok_or_else(ApiError::not_found)?;
    Ok(Proto(snapshot))
}

fn rate_limit(state: &AppState, user: Uuid) -> ApiResult<()> {
    if state.limiters.create_conversation.check(&user) {
        Ok(())
    } else {
        Err(ApiError::rate_limited())
    }
}

pub async fn create(
    State(state): State<AppState>,
    Db(pool): Db,
    me: AuthUser,
    Proto(req): Proto<v1::CreateGroupRequest>,
) -> ApiResult<Proto<v1::Conversation>> {
    proto::validate_group_title(&req.title)?;
    if req.usernames.is_empty() || req.usernames.len() >= proto::MAX_GROUP_MEMBERS {
        return Err(ApiError::bad_request("provide 1..99 other group members"));
    }
    rate_limit(&state, me.user_id)?;
    let mut tx = pool.begin().await?;
    let mut ids = HashSet::from([me.user_id]);
    for username in &req.usernames {
        if !ids.insert(resolve(&mut tx, username).await?) {
            return Err(ApiError::bad_request(
                "group members must be distinct, excluding creator",
            ));
        }
    }
    let conv = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO conversations (id, kind, title, created_by) VALUES ($1, 'group', $2, $3)",
    )
    .bind(conv)
    .bind(req.title.trim())
    .bind(me.user_id)
    .execute(&mut *tx)
    .await?;
    for id in &ids {
        sqlx::query(
            "INSERT INTO conversation_members (conversation_id, user_id, role) VALUES ($1, $2, $3)",
        )
        .bind(conv)
        .bind(id)
        .bind(if *id == me.user_id {
            "creator"
        } else {
            "member"
        })
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    finish(
        &state,
        &pool,
        me.user_id,
        conv,
        &ids.into_iter().collect::<Vec<_>>(),
    )
    .await
}

pub async fn rename(
    State(state): State<AppState>,
    Db(pool): Db,
    me: AuthUser,
    Path(id): Path<String>,
    Proto(req): Proto<v1::RenameGroupRequest>,
) -> ApiResult<Proto<v1::Conversation>> {
    let conv = parse_id(&id)?;
    proto::validate_group_title(&req.title)?;
    rate_limit(&state, me.user_id)?;
    let mut tx = pool.begin().await?;
    creator(&mut tx, conv, me.user_id).await?;
    sqlx::query("UPDATE conversations SET title = $2 WHERE id = $1")
        .bind(conv)
        .bind(req.title.trim())
        .execute(&mut *tx)
        .await?;
    let recipients = members(&mut tx, conv).await?;
    tx.commit().await?;
    finish(&state, &pool, me.user_id, conv, &recipients).await
}

pub async fn add_member(
    State(state): State<AppState>,
    Db(pool): Db,
    me: AuthUser,
    Path(id): Path<String>,
    Proto(req): Proto<v1::AddGroupMemberRequest>,
) -> ApiResult<Proto<v1::Conversation>> {
    let conv = parse_id(&id)?;
    rate_limit(&state, me.user_id)?;
    let mut tx = pool.begin().await?;
    creator(&mut tx, conv, me.user_id).await?;
    let user = resolve(&mut tx, &req.username).await?;
    let mut recipients = members(&mut tx, conv).await?;
    if !recipients.contains(&user) {
        if recipients.len() >= proto::MAX_GROUP_MEMBERS {
            return Err(ApiError::new(
                v1::ApiErrorCode::Conflict,
                "group is full (100 members)",
            ));
        }
        // Keep original receipt cursors on re-add; all existing history is accessible.
        sqlx::query(
            "INSERT INTO conversation_members (conversation_id, user_id, role)
             VALUES ($1, $2, 'member') ON CONFLICT (conversation_id, user_id)
             DO UPDATE SET left_at = NULL, joined_at = now()",
        )
        .bind(conv)
        .bind(user)
        .execute(&mut *tx)
        .await?;
        recipients.push(user);
    }
    tx.commit().await?;
    finish(&state, &pool, me.user_id, conv, &recipients).await
}

pub async fn remove_member(
    State(state): State<AppState>,
    Db(pool): Db,
    me: AuthUser,
    Path((id, user)): Path<(String, String)>,
) -> ApiResult<Proto<v1::Conversation>> {
    let (conv, user) = (parse_id(&id)?, parse_id(&user)?);
    rate_limit(&state, me.user_id)?;
    let mut tx = pool.begin().await?;
    creator(&mut tx, conv, me.user_id).await?;
    if user == me.user_id {
        return Err(ApiError::forbidden("cannot remove the group creator"));
    }
    let known: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM conversation_members WHERE conversation_id = $1 AND user_id = $2)",
    )
    .bind(conv)
    .bind(user)
    .fetch_one(&mut *tx)
    .await?;
    if !known {
        return Err(ApiError::not_found());
    }
    let mut recipients = members(&mut tx, conv).await?;
    sqlx::query(
        "UPDATE conversation_members SET left_at = now()
         WHERE conversation_id = $1 AND user_id = $2 AND left_at IS NULL",
    )
    .bind(conv)
    .bind(user)
    .execute(&mut *tx)
    .await?;
    // Include a previously removed user too: a retry repairs a missed invalidation.
    if !recipients.contains(&user) {
        recipients.push(user);
    }
    tx.commit().await?;
    finish(&state, &pool, me.user_id, conv, &recipients).await
}
