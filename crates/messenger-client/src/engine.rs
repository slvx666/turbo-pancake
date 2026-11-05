//! Sync engine (ADR 0013 §2–4): keeps the cache equal to the server.
//!
//! * `sync` (after every connect): conversation list, then the change feed of each
//!   conversation from its resume cursor, receipts, then pending operations.
//! * Live events: a message event at or below the cursor is a duplicate (dropped), the
//!   next one advances the cursor, one further ahead means something was missed and the
//!   feed is fetched from the cursor. Applying a message twice is harmless (newer
//!   `change_seq` wins), so races between events, answers and pages are safe.
//! * Writes are queued as pending operations first (optimistic UI: pending → sent, or
//!   failed with the server's reason) and retried after reconnects and restarts; the
//!   `client_nonce` makes a retried send idempotent on the server.

use std::collections::BTreeSet;

use turbo_pancake_protocol::{self as proto, v1};
use v1::server_envelope::Body as S;

use crate::api::{Api, ApiError};
use crate::cache::{Cache, CacheError, Op, PendingOp, StoredMember};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Api(#[from] ApiError),
    #[error(transparent)]
    Cache(#[from] CacheError),
    #[error("{0}")]
    Invalid(String),
}

impl Error {
    pub fn is_unauthenticated(&self) -> bool {
        matches!(self, Error::Api(e) if e.is_unauthenticated())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// What the UI should redraw.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Changes {
    pub list: bool,
    pub conversations: BTreeSet<String>,
    /// This session was revoked: sign out.
    pub signed_out: bool,
    /// The open conversation disappeared from the authoritative list.
    pub closed: Option<String>,
}

impl Changes {
    fn conv(&mut self, id: &str) {
        self.list = true;
        self.conversations.insert(id.to_owned());
    }

    pub fn merge(&mut self, other: Changes) {
        self.list |= other.list;
        self.signed_out |= other.signed_out;
        self.conversations.extend(other.conversations);
        if other.closed.is_some() {
            self.closed = other.closed;
        }
    }

    pub fn is_empty(&self) -> bool {
        !self.list && self.conversations.is_empty() && !self.signed_out && self.closed.is_none()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConversationView {
    pub id: String,
    pub title: String,
    pub kind: v1::ConversationKind,
    pub members: Vec<StoredMember>,
    pub can_manage: bool,
    /// Direct conversations: the other member.
    pub peer_user_id: String,
    pub peer_username: String,
    pub online: bool,
    pub unread: i64,
    pub last_text: String,
    pub last_at_ms: i64,
    pub can_send: bool,
    pub blocked_by_me: bool,
    pub draft: String,
    pub draft_reply_to: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MessageState {
    /// Someone else's message.
    Received,
    /// Queued, not yet confirmed by the server.
    Pending,
    /// The server refused it (reason); can be retried or discarded.
    Failed(String),
    Sent,
    Delivered,
    Read,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReplyView {
    pub message_id: String,
    pub author_name: String,
    /// First line of the replied message ("" when deleted or not cached).
    pub text: String,
    pub deleted: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MessageView {
    /// Message id, or the nonce of a pending send.
    pub key: String,
    pub message_id: String,
    pub author_id: String,
    pub author_name: String,
    pub mine: bool,
    pub body: String,
    pub reply: Option<ReplyView>,
    pub created_at_ms: i64,
    pub edited: bool,
    pub deleted: bool,
    pub state: MessageState,
    pub server_sequence: i64,
    /// Nonce of the pending operation shown on this message (retry / discard), or "".
    pub pending_nonce: String,
}

/// Feed page size used for catching up.
const PAGE: u32 = 500;

pub struct Engine {
    pub api: Api,
    pub cache: Cache,
    pub me: v1::User,
    /// The conversation on screen: its new messages count as read.
    open: Option<String>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Engine {
    pub fn new(api: Api, cache: Cache, me: v1::User) -> Self {
        Self {
            api,
            cache,
            me,
            open: None,
        }
    }

    pub fn open_conversation(&self) -> Option<&str> {
        self.open.as_deref()
    }

    /// Full resync: list, feeds, receipts, pending operations.
    pub async fn sync(&mut self) -> Result<Changes> {
        let mut ch = self.refresh_list().await?;
        for id in self.cache.conversation_ids()? {
            ch.merge(self.catch_up(&id).await?);
        }
        ch.merge(self.flush_pending().await?);
        Ok(ch)
    }

    async fn refresh_list(&mut self) -> Result<Changes> {
        let list = self.api.conversations().await?;
        let ids: Vec<String> = list
            .conversations
            .iter()
            .map(|c| c.conversation_id.clone())
            .collect();
        let mut rejoined = BTreeSet::new();
        for c in &list.conversations {
            if c.kind == v1::ConversationKind::Group as i32 {
                let old_revision = self
                    .cache
                    .members(&c.conversation_id)?
                    .into_iter()
                    .find(|m| m.user_id == self.me.user_id)
                    .map(|m| m.membership_revision);
                let new_revision = c
                    .members
                    .iter()
                    .find(|m| {
                        m.user
                            .as_ref()
                            .is_some_and(|u| u.user_id == self.me.user_id)
                    })
                    .map(|m| m.membership_revision);
                if old_revision.is_some() && old_revision != new_revision {
                    self.cache.reset_conversation(&c.conversation_id)?;
                    rejoined.insert(c.conversation_id.clone());
                }
            }
            self.cache.upsert_conversation(c)?;
        }
        let removed: Vec<_> = self
            .cache
            .conversation_ids()?
            .into_iter()
            .filter(|id| !ids.contains(id))
            .collect();
        self.cache.retain_conversations(&ids)?;
        let closed = self
            .open
            .as_ref()
            .filter(|id| !ids.contains(id) || rejoined.contains(*id))
            .cloned();
        if closed.is_some() {
            self.open = None;
        }
        Ok(Changes {
            list: true,
            conversations: ids.into_iter().chain(removed).collect(),
            signed_out: false,
            closed,
        })
    }

    /// Applies the change feed after the resume cursor, then acknowledges delivery.
    async fn catch_up(&mut self, conv: &str) -> Result<Changes> {
        let mut ch = Changes::default();
        let Some(c) = self.cache.conversation(conv)? else {
            return Ok(ch);
        };
        let mut cursor = c.applied_change_seq;
        if c.server_change_seq > cursor || cursor == 0 {
            loop {
                let page = match self.api.changes(conv, cursor, PAGE).await {
                    Ok(page) => page,
                    Err(e) if e.code() == Some(v1::ApiErrorCode::NotFound) => {
                        ch.merge(self.refresh_list().await?);
                        return Ok(ch);
                    }
                    Err(e) => return Err(e.into()),
                };
                for m in &page.messages {
                    if self.cache.apply_message(m)? {
                        ch.conv(conv);
                    }
                    cursor = cursor.max(m.change_seq);
                }
                if !page.has_more {
                    cursor = cursor.max(page.change_seq);
                    break;
                }
            }
            self.cache.set_applied(conv, cursor)?;
        }
        ch.merge(self.ack(conv).await?);
        Ok(ch)
    }

    /// Tells the server what this device has stored (delivered) and shown (read).
    async fn ack(&mut self, conv: &str) -> Result<Changes> {
        let mut ch = Changes::default();
        let newest = self.cache.max_sequence(conv)?;
        let Some(mine) = self
            .cache
            .members(conv)?
            .into_iter()
            .find(|m| m.user_id == self.me.user_id)
        else {
            return Ok(ch);
        };
        let read = if self.open.as_deref() == Some(conv) {
            newest
        } else {
            mine.read_sequence
        };
        if newest > mine.delivered_sequence || read > mine.read_sequence {
            let r = match self.api.mark_read(conv, newest, read).await {
                Ok(r) => r,
                Err(e) if e.code() == Some(v1::ApiErrorCode::NotFound) => {
                    return self.refresh_list().await;
                }
                Err(e) => return Err(e.into()),
            };
            if self.cache.apply_receipt(&r)? {
                ch.conv(conv);
            }
        }
        Ok(ch)
    }

    /// One live event from the WebSocket.
    pub async fn on_event(&mut self, body: S) -> Result<Changes> {
        let mut ch = Changes::default();
        match body {
            S::MessageCreated(e) | S::MessageEdited(e) | S::MessageDeleted(e) => {
                let Some(m) = e.message else { return Ok(ch) };
                let conv = m.conversation_id.clone();
                if !m.client_nonce.is_empty() {
                    // Our own send confirmed through another path (or another device).
                    self.cache.remove_pending(&m.client_nonce)?;
                }
                match self.cache.conversation(&conv)? {
                    None => {
                        ch.merge(self.refresh_list().await?);
                        ch.merge(self.catch_up(&conv).await?);
                    }
                    Some(c) if m.change_seq <= c.applied_change_seq => {
                        // Duplicate (at-least-once delivery, or already fetched).
                        if !m.client_nonce.is_empty() {
                            ch.conv(&conv);
                        }
                    }
                    Some(c) if m.change_seq == c.applied_change_seq + 1 => {
                        self.cache.apply_message(&m)?;
                        self.cache.set_applied(&conv, m.change_seq)?;
                        ch.conv(&conv);
                        if m.author_id != self.me.user_id {
                            ch.merge(self.ack(&conv).await?);
                        }
                    }
                    Some(_) => {
                        // Gap: fetch what we missed (this message included).
                        self.cache.apply_message(&m)?;
                        ch.conv(&conv);
                        ch.merge(self.catch_up(&conv).await?);
                    }
                }
            }
            S::ReadUpdated(r) => {
                if self.cache.apply_receipt(&r)? {
                    ch.conv(&r.conversation_id);
                }
            }
            S::PresenceChanged(p) => {
                if self.cache.set_online(&p.user_id, p.online)? {
                    ch.list = true;
                    for id in self.cache.conversation_ids()? {
                        if self
                            .cache
                            .members(&id)?
                            .iter()
                            .any(|m| m.user_id == p.user_id)
                        {
                            ch.conversations.insert(id);
                        }
                    }
                }
            }
            S::ConversationUpdated(u) => {
                ch.merge(self.refresh_list().await?);
                ch.merge(self.catch_up(&u.conversation_id).await?);
            }
            S::SessionRevoked(_) => ch.signed_out = true,
            _ => {}
        }
        Ok(ch)
    }

    // ---- Writes ----

    fn queue(
        &mut self,
        op: Op,
        conv: &str,
        message_id: &str,
        body: &str,
        reply_to: &str,
    ) -> Result<String> {
        if self.cache.conversation(conv)?.is_none() {
            return Err(Error::Invalid("unknown conversation".into()));
        }
        let nonce = uuid::Uuid::new_v4().to_string();
        self.cache.add_pending(&PendingOp {
            nonce: nonce.clone(),
            conversation_id: conv.into(),
            op,
            message_id: message_id.into(),
            body: body.into(),
            reply_to_id: reply_to.into(),
            created_at_ms: now_ms(),
            failed: None,
        })?;
        Ok(nonce)
    }

    /// Queues a message (shown as pending at once); `flush_pending` sends it.
    pub fn queue_send(&mut self, conv: &str, body: &str, reply_to: &str) -> Result<String> {
        proto::validate_message_body(body).map_err(|e| Error::Invalid(e.to_string()))?;
        let nonce = self.queue(Op::Send, conv, "", body, reply_to)?;
        self.cache.set_draft(conv, "", "")?;
        Ok(nonce)
    }

    pub fn queue_edit(&mut self, conv: &str, message_id: &str, body: &str) -> Result<String> {
        proto::validate_message_body(body).map_err(|e| Error::Invalid(e.to_string()))?;
        self.queue(Op::Edit, conv, message_id, body, "")
    }

    pub fn queue_delete(&mut self, conv: &str, message_id: &str) -> Result<String> {
        self.queue(Op::Delete, conv, message_id, "", "")
    }

    /// Sends pending operations in order. A network failure stops the run (kept for the
    /// next one); a refusal marks that operation failed and goes on.
    pub async fn flush_pending(&mut self) -> Result<Changes> {
        let mut ch = Changes::default();
        for p in self.cache.pending()? {
            if p.failed.is_some() {
                continue;
            }
            let result = match p.op {
                Op::Send => {
                    self.api
                        .send_message(
                            &p.conversation_id,
                            &v1::SendMessageRequest {
                                client_nonce: p.nonce.clone(),
                                body: p.body.clone(),
                                reply_to_id: p.reply_to_id.clone(),
                            },
                        )
                        .await
                }
                Op::Edit => {
                    self.api
                        .edit_message(&p.conversation_id, &p.message_id, &p.body)
                        .await
                }
                Op::Delete => {
                    self.api
                        .delete_message(&p.conversation_id, &p.message_id)
                        .await
                }
            };
            match result {
                Ok(m) => {
                    self.cache.apply_message(&m)?;
                    self.cache.remove_pending(&p.nonce)?;
                    ch.conv(&p.conversation_id);
                }
                Err(e) if e.is_unauthenticated() => return Err(e.into()),
                Err(e) if e.is_transient() => break,
                Err(e) => {
                    self.cache.fail_pending(&p.nonce, &e.to_string())?;
                    ch.conv(&p.conversation_id);
                }
            }
        }
        Ok(ch)
    }

    pub fn retry(&mut self, nonce: &str) -> Result<bool> {
        Ok(self.cache.retry_pending(nonce)?)
    }

    pub fn discard(&mut self, nonce: &str) -> Result<()> {
        Ok(self.cache.remove_pending(nonce)?)
    }

    /// Marks `conv` as the one on screen (its messages become read).
    pub async fn open(&mut self, conv: Option<&str>) -> Result<Changes> {
        if let Some(id) = conv
            && self.cache.conversation(id)?.is_none()
        {
            self.open = None;
            return Ok(Changes {
                list: true,
                closed: Some(id.to_owned()),
                ..Default::default()
            });
        }
        self.open = conv.map(str::to_owned);
        match conv {
            Some(c) => self.ack(c).await,
            None => Ok(Changes::default()),
        }
    }

    /// Finds a user by exact username and opens (or creates) the direct conversation.
    pub async fn start_direct(&mut self, username: &str) -> Result<(String, Changes)> {
        proto::validate_username(username).map_err(|_| Error::Invalid("no such user".into()))?;
        let user = self.api.lookup(username).await?;
        let conv = self.api.create_direct(&user.user_id).await?;
        self.cache.upsert_conversation(&conv)?;
        let mut ch = Changes::default();
        ch.conv(&conv.conversation_id);
        ch.merge(self.catch_up(&conv.conversation_id).await?);
        Ok((conv.conversation_id, ch))
    }

    pub async fn set_blocked(&mut self, user_id: &str, blocked: bool) -> Result<Changes> {
        if blocked {
            self.api.block(user_id).await?;
        } else {
            self.api.unblock(user_id).await?;
        }
        self.refresh_list().await
    }

    pub async fn create_group(
        &mut self,
        title: &str,
        usernames: &[String],
    ) -> Result<(String, Changes)> {
        proto::validate_group_title(title).map_err(|e| Error::Invalid(e.to_string()))?;
        let conv = self.api.create_group(title, usernames).await?;
        // Refetch instead of trusting a mutation response that another device may
        // already have superseded by the time its HTTP answer arrives.
        let ch = self.sync().await?;
        Ok((conv.conversation_id, ch))
    }

    pub async fn rename_group(&mut self, conv: &str, title: &str) -> Result<Changes> {
        proto::validate_group_title(title).map_err(|e| Error::Invalid(e.to_string()))?;
        self.api.rename_group(conv, title).await?;
        self.refresh_list().await
    }

    pub async fn add_group_member(&mut self, conv: &str, username: &str) -> Result<Changes> {
        self.api.add_group_member(conv, username).await?;
        self.refresh_list().await
    }

    pub async fn remove_group_member(&mut self, conv: &str, user: &str) -> Result<Changes> {
        self.api.remove_group_member(conv, user).await?;
        self.refresh_list().await
    }

    pub fn save_draft(&mut self, conv: &str, body: &str, reply_to: &str) -> Result<()> {
        if self.cache.conversation(conv)?.is_none() {
            return Err(Error::Invalid("unknown conversation".into()));
        }
        Ok(self.cache.set_draft(conv, body, reply_to)?)
    }

    // ---- Views ----

    pub fn conversation_views(&self) -> Result<Vec<ConversationView>> {
        let mut out = Vec::new();
        for c in self.cache.conversations()? {
            let members = self.cache.members(&c.id)?;
            let me = members.iter().find(|m| m.user_id == self.me.user_id);
            let kind = v1::ConversationKind::try_from(c.kind).unwrap_or_default();
            let peer = members
                .iter()
                .find(|m| kind == v1::ConversationKind::Direct && m.user_id != self.me.user_id);
            let my_read = me.map_or(0, |m| m.read_sequence);
            let msgs = self.cache.messages(&c.id, None, 200)?;
            let unread = msgs
                .iter()
                .filter(|m| {
                    m.server_sequence > my_read && m.author_id != self.me.user_id && !m.deleted
                })
                .count() as i64;
            let last = msgs.last();
            let (draft, draft_reply_to) = self.cache.draft(&c.id)?;
            out.push(ConversationView {
                title: if c.title.is_empty() {
                    peer.map(|p| p.display_name.clone()).unwrap_or_default()
                } else {
                    c.title.clone()
                },
                peer_user_id: peer.map(|p| p.user_id.clone()).unwrap_or_default(),
                peer_username: peer.map(|p| p.username.clone()).unwrap_or_default(),
                online: peer.is_some_and(|p| p.online),
                unread,
                last_text: last.map(|m| first_line(&m.body)).unwrap_or_default(),
                last_at_ms: last.map_or(c.created_at_ms, |m| m.created_at_ms),
                can_send: c.can_send,
                blocked_by_me: c.blocked_by_me,
                draft,
                draft_reply_to,
                id: c.id,
                kind,
                can_manage: kind == v1::ConversationKind::Group
                    && me.is_some_and(|m| m.role == v1::MemberRole::Creator as i32),
                members,
            });
        }
        // Most recent activity first.
        out.sort_by(|a, b| b.last_at_ms.cmp(&a.last_at_ms).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    /// The newest `limit` messages of `conv` plus pending operations, oldest first.
    pub fn message_views(&self, conv: &str, limit: u32) -> Result<Vec<MessageView>> {
        let members = self.cache.members(conv)?;
        let name = |id: &str| {
            members
                .iter()
                .find(|m| m.user_id == id)
                .map(|m| m.display_name.clone())
                .unwrap_or_else(|| "?".into())
        };
        // Own message state from the other members' receipts (lowest of them).
        let others: Vec<_> = members
            .iter()
            .filter(|m| m.user_id != self.me.user_id)
            .collect();
        let read = others.iter().map(|m| m.read_sequence).min().unwrap_or(0);
        let delivered = others
            .iter()
            .map(|m| m.delivered_sequence)
            .min()
            .unwrap_or(0);
        let pending = self.cache.pending()?;
        let reply_view = |id: &str| -> Result<Option<ReplyView>> {
            if id.is_empty() {
                return Ok(None);
            }
            Ok(Some(match self.cache.message(id)? {
                Some(r) => ReplyView {
                    message_id: id.into(),
                    author_name: name(&r.author_id),
                    text: first_line(&r.body),
                    deleted: r.deleted,
                },
                None => ReplyView {
                    message_id: id.into(),
                    author_name: String::new(),
                    text: String::new(),
                    deleted: false,
                },
            }))
        };
        let mut out = Vec::new();
        for m in self.cache.messages(conv, None, limit)? {
            let mine = m.author_id == self.me.user_id;
            let mut v = MessageView {
                key: m.message_id.clone(),
                message_id: m.message_id.clone(),
                author_name: name(&m.author_id),
                author_id: m.author_id.clone(),
                mine,
                body: m.body.clone(),
                reply: reply_view(&m.reply_to_id)?,
                created_at_ms: m.created_at_ms,
                edited: m.edited_at_ms > 0,
                deleted: m.deleted,
                state: if !mine {
                    MessageState::Received
                } else if read >= m.server_sequence {
                    MessageState::Read
                } else if delivered >= m.server_sequence {
                    MessageState::Delivered
                } else {
                    MessageState::Sent
                },
                server_sequence: m.server_sequence,
                pending_nonce: String::new(),
            };
            // A queued edit or delete shows its result, marked pending/failed.
            for p in pending.iter().filter(|p| p.message_id == m.message_id) {
                let state = p
                    .failed
                    .clone()
                    .map_or(MessageState::Pending, MessageState::Failed);
                match p.op {
                    Op::Edit => {
                        v.body = p.body.clone();
                        v.edited = true;
                    }
                    Op::Delete => {
                        v.body.clear();
                        v.deleted = true;
                    }
                    Op::Send => {}
                }
                v.state = state;
                v.pending_nonce = p.nonce.clone();
            }
            out.push(v);
        }
        for p in pending
            .iter()
            .filter(|p| p.op == Op::Send && p.conversation_id == conv)
        {
            out.push(MessageView {
                key: p.nonce.clone(),
                message_id: String::new(),
                author_id: self.me.user_id.clone(),
                author_name: self.me.display_name.clone(),
                mine: true,
                body: p.body.clone(),
                reply: reply_view(&p.reply_to_id)?,
                created_at_ms: p.created_at_ms,
                edited: false,
                deleted: false,
                state: p
                    .failed
                    .clone()
                    .map_or(MessageState::Pending, MessageState::Failed),
                server_sequence: 0,
                pending_nonce: p.nonce.clone(),
            });
        }
        Ok(out)
    }
}

fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or("");
    let mut out: String = line.chars().take(80).collect();
    if line.chars().count() > 80 || s.lines().nth(1).is_some() {
        out.push('…');
    }
    out
}
