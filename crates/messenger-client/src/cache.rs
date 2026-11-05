//! Local SQLite cache (spec §7, §10; ADR 0013 §11): conversation snapshots, members with
//! receipts, messages, pending operations, drafts, settings.
//!
//! * One file per server and user; the schema version is `PRAGMA user_version`.
//! * Server data is applied idempotently: a message row is replaced only by a newer
//!   `change_seq`, so events, retries and resync pages can arrive in any order.
//! * Deleted messages keep only their tombstone (empty body); `secure_delete` zeroes the
//!   freed pages so the text does not linger in the file.
//! * A file that fails to open, check or migrate is moved aside (`*.corrupt-<n>`) and a
//!   new cache is built from the server.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};
use turbo_pancake_protocol::v1;

/// Ordered migrations; index + 1 is the schema version they produce.
const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE conversations (
    id                 TEXT PRIMARY KEY,
    kind               INTEGER NOT NULL,
    title              TEXT NOT NULL,
    last_sequence      INTEGER NOT NULL,
    server_change_seq  INTEGER NOT NULL,
    -- Highest change_seq of the feed applied without a gap (resume cursor).
    applied_change_seq INTEGER NOT NULL DEFAULT 0,
    created_at         INTEGER NOT NULL,
    blocked_by_me      INTEGER NOT NULL,
    can_send           INTEGER NOT NULL
);
CREATE TABLE members (
    conversation_id    TEXT NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    user_id            TEXT NOT NULL,
    username           TEXT NOT NULL,
    display_name       TEXT NOT NULL,
    online             INTEGER NOT NULL,
    role               INTEGER NOT NULL,
    delivered_sequence INTEGER NOT NULL,
    read_sequence      INTEGER NOT NULL,
    PRIMARY KEY (conversation_id, user_id)
);
CREATE TABLE messages (
    id              TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    server_sequence INTEGER NOT NULL,
    change_seq      INTEGER NOT NULL,
    author_id       TEXT NOT NULL,
    body            TEXT NOT NULL,
    reply_to_id     TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    edited_at       INTEGER NOT NULL,
    deleted         INTEGER NOT NULL
);
CREATE INDEX messages_order ON messages (conversation_id, server_sequence);
-- Operations not yet confirmed by the server, oldest first (rowid order).
CREATE TABLE pending (
    nonce           TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL,
    op              INTEGER NOT NULL,
    message_id      TEXT NOT NULL,
    body            TEXT NOT NULL,
    reply_to_id     TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    -- NULL = will be retried; otherwise the server refused it (shown as failed).
    failed          TEXT
);
CREATE TABLE drafts (
    conversation_id TEXT PRIMARY KEY,
    body            TEXT NOT NULL,
    reply_to_id     TEXT NOT NULL
);
"#,
    "ALTER TABLE members ADD COLUMN membership_revision INTEGER NOT NULL DEFAULT 1;",
    "ALTER TABLE messages ADD COLUMN author_display_name TEXT NOT NULL DEFAULT '';
     UPDATE conversations SET applied_change_seq = 0 WHERE kind = 2;",
];

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("cache database: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("cache file: {0}")]
    Io(#[from] std::io::Error),
    #[error("cache schema version {0} is newer than this build")]
    TooNew(i64),
}

pub type Result<T> = std::result::Result<T, CacheError>;

/// Kind of a pending operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Send = 1,
    Edit = 2,
    Delete = 3,
}

impl Op {
    fn from_i64(v: i64) -> Self {
        match v {
            2 => Op::Edit,
            3 => Op::Delete,
            _ => Op::Send,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PendingOp {
    pub nonce: String,
    pub conversation_id: String,
    pub op: Op,
    /// Edit/delete target (empty for a send).
    pub message_id: String,
    pub body: String,
    pub reply_to_id: String,
    pub created_at_ms: i64,
    /// The server refused it with this reason; it is not retried.
    pub failed: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredConversation {
    pub id: String,
    pub kind: i32,
    pub title: String,
    pub last_sequence: i64,
    pub server_change_seq: i64,
    pub applied_change_seq: i64,
    pub created_at_ms: i64,
    pub blocked_by_me: bool,
    pub can_send: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredMember {
    pub user_id: String,
    pub username: String,
    pub display_name: String,
    pub online: bool,
    pub role: i32,
    pub delivered_sequence: i64,
    pub read_sequence: i64,
    pub membership_revision: i64,
}

pub struct Cache {
    conn: Connection,
    path: Option<PathBuf>,
}

impl Cache {
    /// Opens (or creates) the cache at `path`; a broken file is moved aside and replaced.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        match Self::open_checked(path) {
            Ok(c) => Ok(c),
            Err(CacheError::TooNew(v)) => Err(CacheError::TooNew(v)),
            Err(e) => {
                tracing::warn!(error = %e, "cache unusable, rebuilding it from the server");
                let mut n = 1;
                let aside = loop {
                    let p = path.with_extension(format!("corrupt-{n}"));
                    if !p.exists() {
                        break p;
                    }
                    n += 1;
                };
                std::fs::rename(path, &aside)?;
                for ext in ["sqlite3-wal", "sqlite3-shm"] {
                    let _ = std::fs::remove_file(path.with_extension(ext));
                }
                Self::open_checked(path)
            }
        }
    }

    /// An in-memory cache (tests, or when no cache directory is available).
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let mut c = Self { conn, path: None };
        c.prepare()?;
        Ok(c)
    }

    fn open_checked(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        let mut c = Self {
            conn,
            path: Some(path.to_owned()),
        };
        let ok: String = c.conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if ok != "ok" {
            return Err(CacheError::Sql(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
                Some(ok),
            )));
        }
        c.prepare()?;
        Ok(c)
    }

    fn prepare(&mut self) -> Result<()> {
        self.conn.execute_batch(
            "PRAGMA foreign_keys = ON; PRAGMA secure_delete = ON; PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;",
        )?;
        let version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let latest = MIGRATIONS.len() as i64;
        if version > latest {
            return Err(CacheError::TooNew(version));
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
            tx.commit()?;
        }
        Ok(())
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---- Conversations ----

    /// Stores the server's view of a conversation, keeping the local resume cursor.
    pub fn upsert_conversation(&mut self, c: &v1::Conversation) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO conversations (id, kind, title, last_sequence, server_change_seq,
                                        created_at, blocked_by_me, can_send)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (id) DO UPDATE SET kind = excluded.kind, title = excluded.title,
                 last_sequence = MAX(last_sequence, excluded.last_sequence),
                 server_change_seq = MAX(server_change_seq, excluded.server_change_seq),
                 blocked_by_me = excluded.blocked_by_me, can_send = excluded.can_send",
            params![
                c.conversation_id,
                c.kind,
                c.title,
                c.last_sequence,
                c.change_seq,
                c.created_at_ms,
                c.blocked_by_me,
                c.can_send
            ],
        )?;
        tx.execute(
            "DELETE FROM members WHERE conversation_id = ?1",
            [&c.conversation_id],
        )?;
        for m in &c.members {
            let u = m.user.clone().unwrap_or_default();
            tx.execute(
                "INSERT INTO members (conversation_id, user_id, username, display_name, online,
                                      role, delivered_sequence, read_sequence, membership_revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    c.conversation_id,
                    u.user_id,
                    u.username,
                    u.display_name,
                    u.online,
                    m.role,
                    m.delivered_sequence,
                    m.read_sequence,
                    m.membership_revision
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Drops conversations the server no longer lists (left / removed).
    pub fn retain_conversations(&self, ids: &[String]) -> Result<usize> {
        let all = self.conversation_ids()?;
        let tx = self.conn.unchecked_transaction()?;
        let mut n = 0;
        for id in all.iter().filter(|id| !ids.contains(id)) {
            // Pending/drafts predate group removal and have no FK. Purge explicitly:
            // a later re-add must not silently send text queued before removal.
            tx.execute("DELETE FROM pending WHERE conversation_id = ?1", [id])?;
            tx.execute("DELETE FROM drafts WHERE conversation_id = ?1", [id])?;
            n += tx.execute("DELETE FROM conversations WHERE id = ?1", [id])?;
        }
        tx.commit()?;
        Ok(n)
    }

    pub fn conversation_ids(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT id FROM conversations")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Discard the previous membership's local state before rebuilding its snapshot.
    pub fn reset_conversation(&self, id: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM pending WHERE conversation_id = ?1", [id])?;
        tx.execute("DELETE FROM drafts WHERE conversation_id = ?1", [id])?;
        tx.execute("DELETE FROM conversations WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn conversation(&self, id: &str) -> Result<Option<StoredConversation>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, kind, title, last_sequence, server_change_seq, applied_change_seq,
                        created_at, blocked_by_me, can_send
                   FROM conversations WHERE id = ?1",
                [id],
                Self::conv_row,
            )
            .optional()?)
    }

    pub fn conversations(&self) -> Result<Vec<StoredConversation>> {
        let mut st = self.conn.prepare(
            "SELECT id, kind, title, last_sequence, server_change_seq, applied_change_seq,
                    created_at, blocked_by_me, can_send
               FROM conversations ORDER BY created_at, id",
        )?;
        let rows = st.query_map([], Self::conv_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn conv_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredConversation> {
        Ok(StoredConversation {
            id: r.get(0)?,
            kind: r.get(1)?,
            title: r.get(2)?,
            last_sequence: r.get(3)?,
            server_change_seq: r.get(4)?,
            applied_change_seq: r.get(5)?,
            created_at_ms: r.get(6)?,
            blocked_by_me: r.get(7)?,
            can_send: r.get(8)?,
        })
    }

    pub fn members(&self, conv: &str) -> Result<Vec<StoredMember>> {
        let mut st = self.conn.prepare(
            "SELECT user_id, username, display_name, online, role, delivered_sequence,
                    read_sequence, membership_revision
               FROM members WHERE conversation_id = ?1 ORDER BY user_id",
        )?;
        let rows = st.query_map([conv], |r| {
            Ok(StoredMember {
                user_id: r.get(0)?,
                username: r.get(1)?,
                display_name: r.get(2)?,
                online: r.get(3)?,
                role: r.get(4)?,
                delivered_sequence: r.get(5)?,
                read_sequence: r.get(6)?,
                membership_revision: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Advances the resume cursor (never backwards).
    pub fn set_applied(&self, conv: &str, change_seq: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE conversations
                SET applied_change_seq = MAX(applied_change_seq, ?2),
                    server_change_seq = MAX(server_change_seq, ?2)
              WHERE id = ?1",
            params![conv, change_seq],
        )?;
        Ok(())
    }

    /// A member's receipt (only goes up).
    pub fn apply_receipt(&self, r: &v1::Receipt) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE members
                SET delivered_sequence = MAX(delivered_sequence, ?3),
                    read_sequence = MAX(read_sequence, ?4)
              WHERE conversation_id = ?1 AND user_id = ?2
                AND (delivered_sequence < ?3 OR read_sequence < ?4)",
            params![
                r.conversation_id,
                r.user_id,
                r.delivered_sequence,
                r.read_sequence
            ],
        )? > 0)
    }

    /// Presence of `user` in every conversation it is a member of.
    pub fn set_online(&self, user: &str, online: bool) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE members SET online = ?2 WHERE user_id = ?1 AND online <> ?2",
            params![user, online],
        )? > 0)
    }

    // ---- Messages ----

    /// Stores a message unless the cache already has the same or a newer version.
    /// Returns whether anything changed.
    pub fn apply_message(&self, m: &v1::ChatMessage) -> Result<bool> {
        if self.conversation(&m.conversation_id)?.is_none() {
            return Ok(false);
        }
        let changed = self.conn.execute(
            "INSERT INTO messages (id, conversation_id, server_sequence, change_seq, author_id,
                                   body, reply_to_id, created_at, edited_at, deleted, author_display_name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT (id) DO UPDATE SET change_seq = excluded.change_seq,
                 body = excluded.body, edited_at = excluded.edited_at,
                 deleted = excluded.deleted, author_display_name = excluded.author_display_name
             WHERE excluded.change_seq > messages.change_seq OR
                 (excluded.change_seq = messages.change_seq AND messages.author_display_name = '' AND excluded.author_display_name <> '')",
            params![
                m.message_id,
                m.conversation_id,
                m.server_sequence,
                m.change_seq,
                m.author_id,
                if m.deleted { "" } else { m.body.as_str() },
                m.reply_to_id,
                m.created_at_ms,
                m.edited_at_ms,
                m.deleted,
                m.author_display_name
            ],
        )? > 0;
        if changed {
            self.conn.execute(
                "UPDATE conversations SET last_sequence = MAX(last_sequence, ?2),
                        server_change_seq = MAX(server_change_seq, ?3)
                  WHERE id = ?1",
                params![m.conversation_id, m.server_sequence, m.change_seq],
            )?;
        }
        Ok(changed)
    }

    /// Newest `limit` messages (oldest first), optionally below `before` server_sequence.
    pub fn messages(
        &self,
        conv: &str,
        before: Option<i64>,
        limit: u32,
    ) -> Result<Vec<v1::ChatMessage>> {
        let mut st = self.conn.prepare(
            "SELECT id, conversation_id, server_sequence, change_seq, author_id, body,
                    reply_to_id, created_at, edited_at, deleted, author_display_name
               FROM messages WHERE conversation_id = ?1 AND server_sequence < ?2
              ORDER BY server_sequence DESC LIMIT ?3",
        )?;
        let rows = st.query_map(
            params![conv, before.unwrap_or(i64::MAX), limit],
            Self::msg_row,
        )?;
        let mut out: Vec<_> = rows.collect::<rusqlite::Result<_>>()?;
        out.reverse();
        Ok(out)
    }

    pub fn message(&self, id: &str) -> Result<Option<v1::ChatMessage>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, conversation_id, server_sequence, change_seq, author_id, body,
                        reply_to_id, created_at, edited_at, deleted, author_display_name
                   FROM messages WHERE id = ?1",
                [id],
                Self::msg_row,
            )
            .optional()?)
    }

    /// Highest stored server_sequence of a conversation (0 = none).
    pub fn max_sequence(&self, conv: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(server_sequence), 0) FROM messages WHERE conversation_id = ?1",
            [conv],
            |r| r.get(0),
        )?)
    }

    pub fn message_count(&self, conv: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1",
            [conv],
            |r| r.get(0),
        )?)
    }

    fn msg_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<v1::ChatMessage> {
        Ok(v1::ChatMessage {
            message_id: r.get(0)?,
            conversation_id: r.get(1)?,
            server_sequence: r.get(2)?,
            change_seq: r.get(3)?,
            author_id: r.get(4)?,
            author_display_name: r.get(10)?,
            client_nonce: String::new(),
            body: r.get(5)?,
            reply_to_id: r.get(6)?,
            created_at_ms: r.get(7)?,
            edited_at_ms: r.get(8)?,
            deleted: r.get(9)?,
        })
    }

    // ---- Pending operations ----

    pub fn add_pending(&self, p: &PendingOp) -> Result<()> {
        self.conn.execute(
            "INSERT INTO pending (nonce, conversation_id, op, message_id, body, reply_to_id,
                                  created_at, failed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                p.nonce,
                p.conversation_id,
                p.op as i64,
                p.message_id,
                p.body,
                p.reply_to_id,
                p.created_at_ms,
                p.failed
            ],
        )?;
        Ok(())
    }

    /// All pending operations, oldest first.
    pub fn pending(&self) -> Result<Vec<PendingOp>> {
        let mut st = self.conn.prepare(
            "SELECT nonce, conversation_id, op, message_id, body, reply_to_id, created_at, failed
               FROM pending ORDER BY rowid",
        )?;
        let rows = st.query_map([], |r| {
            Ok(PendingOp {
                nonce: r.get(0)?,
                conversation_id: r.get(1)?,
                op: Op::from_i64(r.get(2)?),
                message_id: r.get(3)?,
                body: r.get(4)?,
                reply_to_id: r.get(5)?,
                created_at_ms: r.get(6)?,
                failed: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn remove_pending(&self, nonce: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM pending WHERE nonce = ?1", [nonce])?;
        Ok(())
    }

    pub fn fail_pending(&self, nonce: &str, reason: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE pending SET failed = ?2 WHERE nonce = ?1",
            params![nonce, reason],
        )?;
        Ok(())
    }

    /// Makes a failed operation pending again (user pressed "retry").
    pub fn retry_pending(&self, nonce: &str) -> Result<bool> {
        Ok(self
            .conn
            .execute("UPDATE pending SET failed = NULL WHERE nonce = ?1", [nonce])?
            > 0)
    }

    // ---- Drafts ----

    pub fn draft(&self, conv: &str) -> Result<(String, String)> {
        Ok(self
            .conn
            .query_row(
                "SELECT body, reply_to_id FROM drafts WHERE conversation_id = ?1",
                [conv],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or_default())
    }

    pub fn set_draft(&self, conv: &str, body: &str, reply_to: &str) -> Result<()> {
        if body.is_empty() && reply_to.is_empty() {
            self.conn
                .execute("DELETE FROM drafts WHERE conversation_id = ?1", [conv])?;
        } else {
            self.conn.execute(
                "INSERT INTO drafts (conversation_id, body, reply_to_id) VALUES (?1, ?2, ?3)
                 ON CONFLICT (conversation_id) DO UPDATE
                    SET body = excluded.body, reply_to_id = excluded.reply_to_id",
                params![conv, body, reply_to],
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(id: &str) -> v1::Conversation {
        v1::Conversation {
            conversation_id: id.into(),
            kind: 1,
            members: vec![v1::Member {
                user: Some(v1::User {
                    user_id: "u1".into(),
                    username: "bob".into(),
                    display_name: "Bob".into(),
                    online: true,
                }),
                role: 2,
                delivered_sequence: 0,
                read_sequence: 0,
                membership_revision: 1,
            }],
            can_send: true,
            ..Default::default()
        }
    }

    fn msg(id: &str, seq: i64, change: i64, body: &str) -> v1::ChatMessage {
        v1::ChatMessage {
            message_id: id.into(),
            conversation_id: "c".into(),
            server_sequence: seq,
            change_seq: change,
            author_id: "u1".into(),
            body: body.into(),
            ..Default::default()
        }
    }

    #[test]
    fn s5_cache_migrates_without_losing_history_drafts_or_pending() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute_batch(
            "INSERT INTO conversations VALUES ('c', 1, '', 1, 1, 1, 1, 0, 1);
             INSERT INTO members VALUES ('c', 'u1', 'bob', 'Bob', 0, 2, 1, 1);
             INSERT INTO messages VALUES ('m', 'c', 1, 1, 'u1', 'history', '', 1, 0, 0);
             INSERT INTO pending VALUES ('p', 'c', 1, '', 'queued', '', 1, NULL);
             INSERT INTO drafts VALUES ('c', 'draft', '');",
        )
        .unwrap();
        let mut cache = Cache { conn, path: None };
        cache.prepare().unwrap();
        assert_eq!(cache.members("c").unwrap()[0].membership_revision, 1);
        assert_eq!(
            cache.conversation("c").unwrap().unwrap().applied_change_seq,
            1
        );
        assert_eq!(cache.message("m").unwrap().unwrap().body, "history");
        assert_eq!(cache.pending().unwrap()[0].body, "queued");
        assert_eq!(cache.draft("c").unwrap().0, "draft");
        cache.prepare().unwrap();
        assert_eq!(cache.pending().unwrap().len(), 1, "reopening is idempotent");
    }

    #[test]
    fn messages_apply_idempotently_in_any_order() {
        let mut c = Cache::in_memory().unwrap();
        c.upsert_conversation(&conv("c")).unwrap();
        assert!(c.apply_message(&msg("m1", 1, 1, "a")).unwrap());
        assert!(
            !c.apply_message(&msg("m1", 1, 1, "a")).unwrap(),
            "duplicate"
        );
        assert!(c.apply_message(&msg("m1", 1, 3, "edited")).unwrap());
        assert!(
            !c.apply_message(&msg("m1", 1, 2, "older")).unwrap(),
            "stale"
        );
        let mut tomb = msg("m1", 1, 4, "should not be kept");
        tomb.deleted = true;
        assert!(c.apply_message(&tomb).unwrap());
        let m = c.message("m1").unwrap().unwrap();
        assert!(m.deleted && m.body.is_empty());
        // Unknown conversation: ignored (fetched by the sync engine first).
        let mut other = msg("m2", 1, 1, "x");
        other.conversation_id = "nope".into();
        assert!(!c.apply_message(&other).unwrap());
        // Re-listing keeps the cursor.
        c.set_applied("c", 4).unwrap();
        c.upsert_conversation(&conv("c")).unwrap();
        assert_eq!(c.conversation("c").unwrap().unwrap().applied_change_seq, 4);
    }

    #[test]
    fn pending_drafts_receipts() {
        let mut c = Cache::in_memory().unwrap();
        c.upsert_conversation(&conv("c")).unwrap();
        for n in ["a", "b"] {
            c.add_pending(&PendingOp {
                nonce: n.into(),
                conversation_id: "c".into(),
                op: Op::Send,
                message_id: String::new(),
                body: n.into(),
                reply_to_id: String::new(),
                created_at_ms: 0,
                failed: None,
            })
            .unwrap();
        }
        c.fail_pending("a", "blocked").unwrap();
        let p = c.pending().unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].failed.as_deref(), Some("blocked"));
        assert!(c.retry_pending("a").unwrap());
        c.remove_pending("b").unwrap();
        assert_eq!(c.pending().unwrap().len(), 1);
        c.set_draft("c", "half-typed", "").unwrap();
        assert_eq!(c.draft("c").unwrap().0, "half-typed");
        c.set_draft("c", "", "").unwrap();
        assert_eq!(c.draft("c").unwrap(), Default::default());
        let r = v1::Receipt {
            conversation_id: "c".into(),
            user_id: "u1".into(),
            delivered_sequence: 5,
            read_sequence: 3,
        };
        assert!(c.apply_receipt(&r).unwrap());
        assert!(!c.apply_receipt(&r).unwrap());
        assert!(c.set_online("u1", false).unwrap());
        let m = &c.members("c").unwrap()[0];
        assert_eq!(
            (m.delivered_sequence, m.read_sequence, m.online),
            (5, 3, false)
        );
    }

    #[test]
    fn corrupted_file_is_moved_aside_and_rebuilt() {
        // Next to the test binary (the build directory), not the system temp dir.
        let exe = std::env::current_exe().unwrap();
        let dir = exe
            .parent()
            .unwrap()
            .join(format!("tp-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("u.sqlite3");
        {
            let mut c = Cache::open(&path).unwrap();
            c.upsert_conversation(&conv("c")).unwrap();
        }
        // Reopen: data kept, schema not re-applied.
        assert_eq!(
            Cache::open(&path).unwrap().conversation_ids().unwrap(),
            vec!["c"]
        );
        std::fs::write(&path, b"this is not a database, just garbage bytes......").unwrap();
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
        let c = Cache::open(&path).unwrap();
        assert!(c.conversation_ids().unwrap().is_empty(), "fresh cache");
        assert!(
            path.with_extension("corrupt-1").exists(),
            "broken file kept aside"
        );
        drop(c);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
