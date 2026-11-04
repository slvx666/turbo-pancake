-- S5: accounts, sessions, direct conversations, messages, receipts, blocks (ADR 0013).
-- Logical model: spec §10. Group-specific columns (title, roles) are used from S6.

CREATE TABLE users (
    id               uuid PRIMARY KEY,
    -- Normalised (lowercase) username; unique and immutable reference for lookups.
    username_key     text NOT NULL UNIQUE,
    username_display text NOT NULL,
    display_name     text NOT NULL,
    -- Argon2id PHC string (algorithm, parameters, salt, hash).
    password_hash    text NOT NULL,
    created_at       timestamptz NOT NULL DEFAULT now(),
    status           text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'disabled')),
    CHECK (lower(username_display) = username_key)
);

CREATE TABLE sessions (
    id           uuid PRIMARY KEY,
    user_id      uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- SHA-256 of the token; the token itself is never stored.
    token_hash   bytea NOT NULL UNIQUE,
    device_id    text NOT NULL DEFAULT '',
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL,
    revoked_at   timestamptz
);
CREATE INDEX sessions_active_by_user ON sessions (user_id) WHERE revoked_at IS NULL;

CREATE TABLE conversations (
    id          uuid PRIMARY KEY,
    kind        text NOT NULL CHECK (kind IN ('direct', 'group')),
    title       text NOT NULL DEFAULT '',
    created_by  uuid NOT NULL REFERENCES users (id),
    created_at  timestamptz NOT NULL DEFAULT now(),
    -- Direct conversations: the two members, smaller id first (one conversation per pair).
    direct_low  uuid REFERENCES users (id),
    direct_high uuid REFERENCES users (id),
    -- Counters bumped under the row lock by every message write (ADR 0013 §2).
    message_seq bigint NOT NULL DEFAULT 0,
    change_seq  bigint NOT NULL DEFAULT 0,
    CHECK ((kind = 'direct') = (direct_low IS NOT NULL AND direct_high IS NOT NULL)),
    CHECK (direct_low < direct_high)
);
CREATE UNIQUE INDEX conversations_direct_pair ON conversations (direct_low, direct_high)
    WHERE kind = 'direct';

CREATE TABLE conversation_members (
    conversation_id    uuid NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    user_id            uuid NOT NULL REFERENCES users (id),
    role               text NOT NULL CHECK (role IN ('creator', 'member')),
    joined_at          timestamptz NOT NULL DEFAULT now(),
    left_at            timestamptz,
    delivered_sequence bigint NOT NULL DEFAULT 0,
    read_sequence      bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (conversation_id, user_id)
);
CREATE INDEX conversation_members_by_user ON conversation_members (user_id)
    WHERE left_at IS NULL;

CREATE TABLE messages (
    id              uuid PRIMARY KEY,
    conversation_id uuid NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    server_sequence bigint NOT NULL,
    change_seq      bigint NOT NULL,
    author_id       uuid NOT NULL REFERENCES users (id),
    client_nonce    text NOT NULL,
    -- NULL once deleted for everyone (the row stays as a tombstone for replies and sync).
    body            text,
    reply_to_id     uuid REFERENCES messages (id),
    created_at      timestamptz NOT NULL DEFAULT now(),
    edited_at       timestamptz,
    deleted_at      timestamptz,
    UNIQUE (conversation_id, server_sequence),
    UNIQUE (author_id, client_nonce),
    CHECK ((deleted_at IS NULL) = (body IS NOT NULL))
);
CREATE INDEX messages_by_change ON messages (conversation_id, change_seq);

CREATE TABLE blocks (
    blocker_user_id uuid NOT NULL REFERENCES users (id),
    blocked_user_id uuid NOT NULL REFERENCES users (id),
    created_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (blocker_user_id, blocked_user_id),
    CHECK (blocker_user_id <> blocked_user_id)
);
CREATE INDEX blocks_by_blocked ON blocks (blocked_user_id);
