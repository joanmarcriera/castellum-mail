-- Thread projection (slice 6a). `threads` is a rebuildable projection over
-- `messages`, linked by `message_references` (the rfc822 message-ids each message
-- references). messages.thread_id (added in 0002) points a message at its thread.

CREATE TABLE message_references (
    message_id     TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    ref_message_id TEXT NOT NULL,
    ordinal        INTEGER NOT NULL,
    PRIMARY KEY (message_id, ordinal)
);
CREATE INDEX idx_message_references_ref ON message_references(ref_message_id);

CREATE TABLE threads (
    id               TEXT PRIMARY KEY,
    account_id       TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    subject          TEXT,
    first_message_at INTEGER,
    last_message_at  INTEGER,
    message_count    INTEGER NOT NULL DEFAULT 0,
    unread_count     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_threads_account ON threads(account_id, last_message_at DESC, id);

CREATE INDEX idx_messages_thread ON messages(thread_id);
