//! Database consistency check (S8 step 5): `turbo-pancake-app-server check-db`, run after a
//! restore (and at any time on a live database: it only reads). Every check is a query
//! that counts rows breaking one invariant of ADR 0013 / ADR 0015 that the schema's
//! constraints do not already enforce; the report also lists row counts and per-table
//! fingerprints (md5 over the ordered rows) to compare two copies of the same data.

use serde_json::{Value, json};
use sqlx::PgPool;

/// (name, query returning one bigint: the number of violations).
const CHECKS: &[(&str, &str)] = &[
    (
        "message_sequences_gap_free",
        "SELECT count(*) FROM (
           SELECT conversation_id FROM messages GROUP BY conversation_id
           HAVING count(*) <> max(server_sequence) OR min(server_sequence) <> 1) v",
    ),
    (
        "conversation_message_seq_matches",
        "SELECT count(*) FROM conversations c
          WHERE c.message_seq <> COALESCE(
            (SELECT max(server_sequence) FROM messages m WHERE m.conversation_id = c.id), 0)",
    ),
    (
        "conversation_change_seq_covers_messages",
        "SELECT count(*) FROM conversations c
          WHERE c.change_seq < COALESCE(
            (SELECT max(change_seq) FROM messages m WHERE m.conversation_id = c.id), 0)",
    ),
    (
        "message_change_seq_unique",
        "SELECT count(*) FROM (
           SELECT 1 FROM messages GROUP BY conversation_id, change_seq HAVING count(*) > 1) v",
    ),
    (
        "receipts_within_history",
        "SELECT count(*) FROM conversation_members cm JOIN conversations c
              ON c.id = cm.conversation_id
          WHERE cm.delivered_sequence > c.message_seq OR cm.read_sequence > c.message_seq
             OR cm.delivered_sequence < 0 OR cm.read_sequence < 0",
    ),
    (
        "direct_conversations_have_their_two_members",
        "SELECT count(*) FROM conversations c
          WHERE c.kind = 'direct' AND (
            (SELECT count(*) FROM conversation_members m WHERE m.conversation_id = c.id) <> 2
            OR NOT EXISTS (SELECT 1 FROM conversation_members m
                            WHERE m.conversation_id = c.id AND m.user_id = c.direct_low)
            OR NOT EXISTS (SELECT 1 FROM conversation_members m
                            WHERE m.conversation_id = c.id AND m.user_id = c.direct_high))",
    ),
    (
        "groups_have_one_creator_who_created_them",
        "SELECT count(*) FROM conversations c
          WHERE c.kind = 'group' AND (
            (SELECT count(*) FROM conversation_members m
              WHERE m.conversation_id = c.id AND m.role = 'creator') <> 1
            OR NOT EXISTS (SELECT 1 FROM conversation_members m
              WHERE m.conversation_id = c.id AND m.role = 'creator'
                AND m.user_id = c.created_by))",
    ),
    (
        "groups_within_100_active_members",
        "SELECT count(*) FROM (
           SELECT conversation_id FROM conversation_members WHERE left_at IS NULL
            GROUP BY conversation_id HAVING count(*) > 100) v",
    ),
    (
        "replies_stay_in_their_conversation",
        "SELECT count(*) FROM messages m JOIN messages r ON r.id = m.reply_to_id
          WHERE r.conversation_id <> m.conversation_id",
    ),
    (
        "authors_were_members",
        "SELECT count(*) FROM messages m
          WHERE NOT EXISTS (SELECT 1 FROM conversation_members cm
                             WHERE cm.conversation_id = m.conversation_id
                               AND cm.user_id = m.author_id)",
    ),
];

/// Tables and the order that makes their fingerprint deterministic.
const TABLES: &[(&str, &str)] = &[
    ("users", "id"),
    ("sessions", "id"),
    ("conversations", "id"),
    ("conversation_members", "conversation_id, user_id"),
    ("messages", "conversation_id, server_sequence"),
    ("blocks", "blocker_user_id, blocked_user_id"),
];

/// Runs every check. `ok` is true when no invariant is broken.
pub async fn check(pool: &PgPool) -> sqlx::Result<Value> {
    let mut checks = serde_json::Map::new();
    let mut broken = 0i64;
    for (name, sql) in CHECKS {
        let n: i64 = sqlx::query_scalar(sql).fetch_one(pool).await?;
        broken += n;
        checks.insert((*name).into(), json!(n));
    }
    let mut tables = serde_json::Map::new();
    for (table, order) in TABLES {
        // Names come from the constant list above, never from input.
        let (rows, md5): (i64, Option<String>) = sqlx::query_as(&format!(
            "SELECT count(*), md5(string_agg(t::text, E'\\n' ORDER BY {order})) FROM {table} t"
        ))
        .fetch_one(pool)
        .await?;
        tables.insert(
            (*table).into(),
            json!({ "rows": rows, "fingerprint": md5.unwrap_or_default() }),
        );
    }
    let deleted: i64 =
        sqlx::query_scalar("SELECT count(*) FROM messages WHERE deleted_at IS NOT NULL")
            .fetch_one(pool)
            .await?;
    let migrations: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE success")
        .fetch_one(pool)
        .await?;
    Ok(json!({
        "ok": broken == 0,
        "violations": broken,
        "checks": checks,
        "tables": tables,
        "deleted_messages": deleted,
        "migrations_applied": migrations,
    }))
}
