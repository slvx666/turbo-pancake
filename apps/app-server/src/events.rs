//! Signed-in connections per user: event fan-out, presence and session revocation
//! (spec §8 presence by live sessions with heartbeat, §11 events; ADR 0013 §8).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::PgPool;
use turbo_pancake_protocol::v1;
use uuid::Uuid;
use v1::server_envelope::Body as S;

use crate::signaling::PeerHandle;

struct Conn {
    id: u64,
    session_id: Uuid,
    handle: PeerHandle,
}

#[derive(Default)]
pub struct Registry {
    users: Mutex<HashMap<Uuid, Vec<Conn>>>,
    next: AtomicU64,
}

impl Registry {
    /// Adds a signed-in connection; returns its id and whether the user just came online.
    pub(crate) fn register(&self, user: Uuid, session_id: Uuid, handle: PeerHandle) -> (u64, bool) {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let mut users = self.users.lock().expect("registry lock");
        let conns = users.entry(user).or_default();
        let first = conns.is_empty();
        conns.push(Conn {
            id,
            session_id,
            handle,
        });
        (id, first)
    }

    /// Removes a connection; returns whether the user went offline.
    pub fn unregister(&self, user: Uuid, conn_id: u64) -> bool {
        let mut users = self.users.lock().expect("registry lock");
        let Some(conns) = users.get_mut(&user) else {
            return false;
        };
        conns.retain(|c| c.id != conn_id);
        if conns.is_empty() {
            users.remove(&user);
            true
        } else {
            false
        }
    }

    pub fn is_online(&self, user: Uuid) -> bool {
        self.users
            .lock()
            .expect("registry lock")
            .contains_key(&user)
    }

    /// Number of users with at least one live connection.
    pub fn online_users(&self) -> usize {
        self.users.lock().expect("registry lock").len()
    }

    /// Sends an event to every connection of `user` (a slow connection is dropped).
    pub fn send(&self, user: Uuid, body: S) {
        let users = self.users.lock().expect("registry lock");
        for c in users.get(&user).into_iter().flatten() {
            c.handle.send(0, body.clone());
        }
    }

    /// Tells the session's connections it ended and closes them.
    pub fn revoke_session(&self, session_id: Uuid) {
        let users = self.users.lock().expect("registry lock");
        for c in users
            .values()
            .flatten()
            .filter(|c| c.session_id == session_id)
        {
            c.handle.send(
                0,
                S::SessionRevoked(v1::SessionRevoked {
                    session_id: session_id.to_string(),
                }),
            );
            c.handle.close();
        }
    }

    /// Revokes every connection of `user` except those of `keep`.
    pub fn revoke_user(&self, user: Uuid, keep: Option<Uuid>) {
        let sessions: Vec<Uuid> = {
            let users = self.users.lock().expect("registry lock");
            users
                .get(&user)
                .into_iter()
                .flatten()
                .map(|c| c.session_id)
                .filter(|s| Some(*s) != keep)
                .collect()
        };
        for s in sessions {
            self.revoke_session(s);
        }
    }
}

/// Users who share a conversation with `user` and are not in a block with them: they
/// see `user`'s presence.
pub async fn contacts(pool: &PgPool, user: Uuid) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar(
        "SELECT DISTINCT m2.user_id
           FROM conversation_members m1
           JOIN conversation_members m2 ON m2.conversation_id = m1.conversation_id
          WHERE m1.user_id = $1 AND m2.user_id <> $1
            AND m1.left_at IS NULL AND m2.left_at IS NULL
            AND NOT EXISTS (SELECT 1 FROM blocks b
                             WHERE (b.blocker_user_id = $1 AND b.blocked_user_id = m2.user_id)
                                OR (b.blocker_user_id = m2.user_id AND b.blocked_user_id = $1))",
    )
    .bind(user)
    .fetch_all(pool)
    .await
}

/// Announces `user`'s presence to its contacts. Sends the state at the time of sending,
/// so quick online/offline flips that race each other still end in the right state.
pub fn announce_presence(pool: PgPool, registry: std::sync::Arc<Registry>, user: Uuid) {
    tokio::spawn(async move {
        match contacts(&pool, user).await {
            Ok(list) => {
                let body = S::PresenceChanged(v1::PresenceChanged {
                    user_id: user.to_string(),
                    online: registry.is_online(user),
                });
                for c in list {
                    registry.send(c, body.clone());
                }
            }
            Err(e) => tracing::warn!(error = %e, "presence fan-out failed"),
        }
    });
}
