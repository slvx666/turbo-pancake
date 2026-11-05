-- Distinguish an uninterrupted membership from remove + re-add while a client is
-- offline. Message cursors alone cannot detect that loss of access.
ALTER TABLE conversation_members
    ADD COLUMN membership_revision bigint NOT NULL DEFAULT 1 CHECK (membership_revision > 0);
