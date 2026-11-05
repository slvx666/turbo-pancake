//! Account/conversation call control (ADR 0015). The control lock always precedes
//! membership/block writes. Media attachment and eviction use that same lock.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use turbo_pancake_protocol::v1::{self, server_envelope::Body as S};
use uuid::Uuid;

use crate::AppState;
use crate::api::{ApiError, ApiResult, AuthUser, Db, Proto, parse_id};
use crate::signaling::PeerHandle;

struct Participant {
    session: Uuid,
    lease: Uuid,
    peer: Option<(String, PeerHandle)>,
    deadline: Option<Instant>,
}

struct Call {
    id: Uuid,
    conversation: Uuid,
    direct: bool,
    started_by: Uuid,
    revision: u64,
    recipients: Vec<Uuid>,
    participants: HashMap<Uuid, Participant>,
    invitation_deadline: Option<Instant>,
}

impl Call {
    fn snapshot(&self, reason: &str) -> v1::ConversationCall {
        let mut participants: Vec<_> = self
            .participants
            .iter()
            .map(|(user, p)| v1::CallParticipant {
                user_id: user.to_string(),
                peer_id: p
                    .peer
                    .as_ref()
                    .map(|(id, _)| id.clone())
                    .unwrap_or_default(),
            })
            .collect();
        participants.sort_by(|a, b| a.user_id.cmp(&b.user_id));
        v1::ConversationCall {
            call_id: self.id.to_string(),
            conversation_id: self.conversation.to_string(),
            revision: self.revision,
            state: if !reason.is_empty() {
                v1::ConversationCallState::Ended
            } else if self.invitation_deadline.is_some() {
                v1::ConversationCallState::Inviting
            } else {
                v1::ConversationCallState::Active
            } as i32,
            participants,
            started_by: self.started_by.to_string(),
            end_reason: reason.into(),
        }
    }

    fn announce(&self, state: &AppState, kind: fn(v1::ConversationCall) -> S, reason: &str) {
        let body = kind(self.snapshot(reason));
        for user in &self.recipients {
            state.registry.send(*user, body.clone());
        }
    }
}

#[derive(Default)]
pub struct Book {
    calls: HashMap<Uuid, Call>,
}

fn room(id: Uuid) -> String {
    format!("call_{id}")
}

impl Book {
    pub(crate) fn add_member(&mut self, state: &AppState, conversation: Uuid, user: Uuid) {
        for call in self
            .calls
            .values_mut()
            .filter(|c| c.conversation == conversation)
        {
            if !call.recipients.contains(&user) {
                call.recipients.push(user);
                state.registry.send(user, S::CallStarted(call.snapshot("")));
            }
        }
    }
    fn evict(state: &AppState, call: Uuid, p: &Participant) {
        if let Some((peer, handle)) = &p.peer {
            state
                .hub
                .evict(&room(call), peer, v1::LeaveReason::Switched);
            if let Some(sfu) = &state.sfu {
                sfu.leave(peer);
            }
            handle.send(
                0,
                S::RoomLeft(v1::RoomLeft {
                    room_id: room(call),
                }),
            );
        }
    }

    fn remove(&mut self, state: &AppState, call_id: Uuid, user: Uuid, reason: &str) {
        let Some(call) = self.calls.get_mut(&call_id) else {
            return;
        };
        let Some(p) = call.participants.remove(&user) else {
            return;
        };
        Self::evict(state, call_id, &p);
        call.revision += 1;
        if call.participants.is_empty() {
            call.announce(state, S::CallEnded, reason);
            self.calls.remove(&call_id);
        } else {
            call.announce(state, S::CallParticipantLeft, "");
        }
    }

    pub(crate) fn remove_member(&mut self, state: &AppState, conversation: Uuid, user: Uuid) {
        let ids: Vec<_> = self
            .calls
            .values_mut()
            .filter(|c| c.conversation == conversation)
            .map(|c| {
                c.recipients.retain(|u| *u != user);
                c.id
            })
            .collect();
        for id in ids {
            self.remove(state, id, user, "removed");
        }
    }

    pub(crate) fn end_conversation(&mut self, state: &AppState, conversation: Uuid, reason: &str) {
        let id = self
            .calls
            .values()
            .find(|c| c.conversation == conversation)
            .map(|c| c.id);
        if let Some(id) = id {
            self.end(state, id, reason);
        }
    }

    fn end(&mut self, state: &AppState, id: Uuid, reason: &str) {
        if let Some(mut call) = self.calls.remove(&id) {
            for p in call.participants.values() {
                Self::evict(state, id, p);
            }
            call.participants.clear();
            call.revision += 1;
            call.announce(state, S::CallEnded, reason);
        }
    }

    pub(crate) fn authorized(&self, user: Uuid, session: Uuid, peer: &str, room_id: &str) -> bool {
        self.calls.values().any(|c| {
            room(c.id) == room_id
                && c.participants.get(&user).is_some_and(|p| {
                    p.session == session && p.peer.as_ref().is_some_and(|(id, _)| id == peer)
                })
        })
    }

    pub(crate) async fn attach(
        &mut self,
        state: &AppState,
        me: &AuthUser,
        peer: &str,
        handle: PeerHandle,
        req: &v1::JoinCallMedia,
    ) -> ApiResult<String> {
        let id = parse_id(&req.call_id)?;
        let lease = parse_id(&req.participation_id)?;
        let conversation = self
            .calls
            .get(&id)
            .ok_or_else(ApiError::not_found)?
            .conversation;
        let pool = state.db.as_ref().ok_or_else(ApiError::unauthenticated)?;
        access(pool, me, conversation).await?;
        let call = self.calls.get_mut(&id).ok_or_else(ApiError::not_found)?;
        let p = call
            .participants
            .get_mut(&me.user_id)
            .filter(|p| p.session == me.session_id && p.lease == lease)
            .ok_or_else(|| ApiError::forbidden("call participation ended"))?;
        if p.deadline.is_some_and(|d| d <= Instant::now()) {
            return Err(ApiError::forbidden("call participation expired"));
        }
        if p.peer.as_ref().is_some_and(|(old, _)| old == peer) {
            return Ok(room(id));
        }
        Self::evict(state, id, p);
        p.peer = Some((peer.into(), handle));
        p.deadline = None;
        call.revision += 1;
        call.announce(state, S::CallParticipantJoined, "");
        Ok(room(id))
    }

    pub(crate) fn disconnect(&mut self, state: &AppState, user: Uuid, peer: &str, leaving: bool) {
        let id = self
            .calls
            .values()
            .find(|c| {
                c.participants
                    .get(&user)
                    .is_some_and(|p| p.peer.as_ref().is_some_and(|(id, _)| id == peer))
            })
            .map(|c| c.id);
        let Some(id) = id else { return };
        if leaving {
            self.remove(state, id, user, "left");
            return;
        }
        let call = self.calls.get_mut(&id).unwrap();
        let p = call.participants.get_mut(&user).unwrap();
        p.peer = None;
        p.deadline = Some(Instant::now() + Duration::from_secs(10));
        call.revision += 1;
        call.announce(state, S::CallParticipantLeft, "");
    }

    fn join(
        &mut self,
        state: &AppState,
        me: &AuthUser,
        id: Uuid,
    ) -> ApiResult<Proto<v1::CallParticipation>> {
        // Retry on the owning session preserves its lease and media connection.
        if let Some(p) = self
            .calls
            .get(&id)
            .and_then(|c| c.participants.get(&me.user_id))
            && p.session == me.session_id
        {
            return Ok(Proto(v1::CallParticipation {
                call: Some(self.calls[&id].snapshot("")),
                participation_id: p.lease.to_string(),
            }));
        }
        let total: usize = self.calls.values().map(|c| c.participants.len()).sum();
        let old = self
            .calls
            .values()
            .find(|c| c.participants.contains_key(&me.user_id))
            .map(|c| c.id);
        if total >= state.config.max_connections && old.is_none() {
            return Err(ApiError::new(
                v1::ApiErrorCode::Unavailable,
                "call capacity reached",
            ));
        }
        if let Some(old) = old {
            // Keep the target call alive if the same account moves devices.
            if old == id {
                let call = self.calls.get_mut(&id).unwrap();
                let p = call.participants.remove(&me.user_id).unwrap();
                Self::evict(state, old, &p);
                call.revision += 1;
                call.announce(state, S::CallParticipantLeft, "");
            } else {
                self.remove(state, old, me.user_id, "switched");
            }
        }
        let call = self.calls.get_mut(&id).ok_or_else(ApiError::not_found)?;
        let fresh = call.revision == 1 && call.participants.is_empty();
        let lease = Uuid::new_v4();
        call.participants.insert(
            me.user_id,
            Participant {
                session: me.session_id,
                lease,
                peer: None,
                deadline: Some(Instant::now() + Duration::from_secs(30)),
            },
        );
        if call.participants.len() > 1 {
            call.invitation_deadline = None;
        }
        call.revision += 1;
        call.announce(
            state,
            if fresh {
                S::CallStarted
            } else {
                S::CallParticipantJoined
            },
            "",
        );
        Ok(Proto(v1::CallParticipation {
            call: Some(call.snapshot("")),
            participation_id: lease.to_string(),
        }))
    }
}

// Called under the control lock. Group removal/block writes acquire it first too.
async fn access(pool: &sqlx::PgPool, me: &AuthUser, conv: Uuid) -> ApiResult<(bool, Vec<Uuid>)> {
    let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions s JOIN users u ON u.id = s.user_id
        WHERE s.id = $1 AND s.user_id = $2 AND s.revoked_at IS NULL AND s.expires_at > now() AND u.status = 'active')")
        .bind(me.session_id).bind(me.user_id).fetch_one(pool).await?;
    if !valid {
        return Err(ApiError::unauthenticated());
    }
    let rows: Vec<(Uuid, String)> = sqlx::query_as("SELECT m.user_id, c.kind FROM conversations c
        JOIN conversation_members m ON m.conversation_id = c.id WHERE c.id = $1 AND m.left_at IS NULL")
        .bind(conv).fetch_all(pool).await?;
    if !rows.iter().any(|(u, _)| *u == me.user_id) {
        return Err(ApiError::not_found());
    }
    let direct = rows[0].1 == "direct";
    let members: Vec<_> = rows.into_iter().map(|(u, _)| u).collect();
    if direct {
        let blocked: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM blocks
            WHERE blocker_user_id = ANY($1) AND blocked_user_id = ANY($1))",
        )
        .bind(&members)
        .fetch_one(pool)
        .await?;
        if blocked {
            return Err(ApiError::forbidden("you cannot call this user"));
        }
    }
    Ok((direct, members))
}

pub async fn create(
    State(state): State<AppState>,
    Db(pool): Db,
    me: AuthUser,
    Proto(req): Proto<v1::CreateCallRequest>,
) -> ApiResult<Proto<v1::CallParticipation>> {
    let conv = parse_id(&req.conversation_id)?;
    if !state.limiters.create_conversation.check(&me.user_id) {
        return Err(ApiError::rate_limited());
    }
    let mut book = state.calls.lock().await;
    let (direct, members) = access(&pool, &me, conv).await?;
    if let Some(id) = book
        .calls
        .values()
        .find(|c| c.conversation == conv)
        .map(|c| c.id)
    {
        return book.join(&state, &me, id);
    }
    if direct
        && book.calls.values().any(|c| {
            c.participants
                .keys()
                .any(|u| *u != me.user_id && members.contains(u))
        })
    {
        return Err(ApiError::new(v1::ApiErrorCode::Conflict, "user is busy"));
    }
    if book.calls.len() >= state.config.max_connections {
        return Err(ApiError::new(
            v1::ApiErrorCode::Unavailable,
            "call capacity reached",
        ));
    }
    let id = Uuid::new_v4();
    book.calls.insert(
        id,
        Call {
            id,
            conversation: conv,
            direct,
            started_by: me.user_id,
            revision: 1,
            recipients: members,
            participants: HashMap::new(),
            invitation_deadline: direct.then(|| Instant::now() + Duration::from_secs(30)),
        },
    );
    let result = book.join(&state, &me, id);
    if result.is_err() {
        book.calls.remove(&id);
    }
    result
}

pub async fn join(
    State(state): State<AppState>,
    Db(pool): Db,
    me: AuthUser,
    Path(id): Path<String>,
) -> ApiResult<Proto<v1::CallParticipation>> {
    let id = parse_id(&id)?;
    let mut book = state.calls.lock().await;
    let conv = book
        .calls
        .get(&id)
        .ok_or_else(ApiError::not_found)?
        .conversation;
    let (_, members) = access(&pool, &me, conv).await?;
    book.calls.get_mut(&id).unwrap().recipients = members;
    book.join(&state, &me, id)
}

pub async fn leave(
    State(state): State<AppState>,
    Db(_pool): Db,
    me: AuthUser,
    Path(id): Path<String>,
    Proto(req): Proto<v1::LeaveCallRequest>,
) -> ApiResult<Proto<v1::Empty>> {
    let (id, lease) = (parse_id(&id)?, parse_id(&req.participation_id)?);
    let mut book = state.calls.lock().await;
    if book
        .calls
        .get(&id)
        .and_then(|c| c.participants.get(&me.user_id))
        .is_some_and(|p| p.session == me.session_id && p.lease == lease)
    {
        book.remove(&state, id, me.user_id, "left");
    }
    Ok(Proto(v1::Empty {}))
}

pub async fn decline(
    State(state): State<AppState>,
    Db(pool): Db,
    me: AuthUser,
    Path(id): Path<String>,
) -> ApiResult<Proto<v1::Empty>> {
    let id = parse_id(&id)?;
    let mut book = state.calls.lock().await;
    if let Some(call) = book.calls.get(&id) {
        access(&pool, &me, call.conversation).await?;
        if !call.direct || call.started_by == me.user_id || call.invitation_deadline.is_none() {
            return Err(ApiError::forbidden(
                "only an invited direct-call recipient can decline",
            ));
        }
        book.end(&state, id, "declined");
    }
    Ok(Proto(v1::Empty {}))
}

pub async fn list(
    State(state): State<AppState>,
    Db(pool): Db,
    me: AuthUser,
) -> ApiResult<Proto<v1::CallList>> {
    let book = state.calls.lock().await;
    let conversations: Vec<Uuid> = sqlx::query_scalar(
        "SELECT conversation_id FROM conversation_members WHERE user_id = $1 AND left_at IS NULL",
    )
    .bind(me.user_id)
    .fetch_all(&pool)
    .await?;
    Ok(Proto(v1::CallList {
        calls: book
            .calls
            .values()
            .filter(|c| conversations.contains(&c.conversation))
            .map(|c| c.snapshot(""))
            .collect(),
    }))
}

pub fn supervise(state: AppState) {
    tokio::spawn(async move {
        let mut stop = state.shutdown.clone();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! { _ = stop.wait_for(|v| *v) => break, _ = tick.tick() => {} }
            let mut book = state.calls.lock().await;
            let sessions: Vec<_> = book
                .calls
                .values()
                .flat_map(|c| c.participants.values().map(|p| p.session))
                .collect();
            let valid: Vec<Uuid> = if let Some(pool) = &state.db {
                match sqlx::query_scalar("SELECT s.id FROM sessions s JOIN users u ON u.id = s.user_id
                    WHERE s.id = ANY($1) AND s.revoked_at IS NULL AND s.expires_at > now() AND u.status = 'active'")
                    .bind(&sessions).fetch_all(pool).await { Ok(v) => v, Err(_) => continue }
            } else {
                Vec::new()
            };
            let now = Instant::now();
            let expired: Vec<_> = book
                .calls
                .values()
                .filter(|c| c.invitation_deadline.is_some_and(|d| d <= now))
                .map(|c| c.id)
                .collect();
            for id in expired {
                book.end(&state, id, "timeout");
            }
            let expired: Vec<_> = book
                .calls
                .values()
                .flat_map(|c| {
                    c.participants.iter().filter_map(|(u, p)| {
                        if !valid.contains(&p.session) {
                            Some((c.id, *u, "revoked"))
                        } else if p.deadline.is_some_and(|d| d <= now) {
                            Some((c.id, *u, "timeout"))
                        } else {
                            None
                        }
                    })
                })
                .collect();
            for (id, user, why) in expired {
                book.remove(&state, id, user, why);
            }
        }
    });
}
