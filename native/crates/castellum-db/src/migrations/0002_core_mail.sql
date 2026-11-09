-- Core mail schema (slice 3). Timestamps that come from mail headers are stored
-- as Unix epoch seconds (INTEGER); bookkeeping timestamps use TEXT datetime('now').
-- This is the write target for IMAP sync (slice 4) and read APIs (slice 5).

CREATE TABLE accounts (
    id           TEXT PRIMARY KEY,
    email        TEXT NOT NULL UNIQUE,
    display_name TEXT,
    provider     TEXT NOT NULL,
    created_at   TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE mailboxes (
    id           TEXT PRIMARY KEY,
    account_id   TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    path         TEXT NOT NULL,
    name         TEXT NOT NULL,
    role         TEXT,
    UNIQUE (account_id, path)
);
CREATE INDEX idx_mailboxes_account ON mailboxes(account_id);

-- One incremental-sync cursor per mailbox (IMAP UIDVALIDITY/UIDNEXT/MODSEQ).
CREATE TABLE sync_cursors (
    account_id     TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    mailbox_id     TEXT NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE,
    uid_validity   INTEGER,
    uid_next       INTEGER,
    highest_modseq INTEGER,
    updated_at     TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (account_id, mailbox_id)
);

CREATE TABLE messages (
    id                 TEXT PRIMARY KEY,
    account_id         TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    mailbox_id         TEXT NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE,
    provider_uid       INTEGER,
    rfc822_message_id  TEXT,
    thread_id          TEXT,
    subject            TEXT,
    from_name          TEXT,
    from_address       TEXT,
    sent_at            INTEGER,   -- Unix epoch seconds
    received_at        INTEGER,
    snippet            TEXT,
    size_bytes         INTEGER,
    has_attachments    INTEGER NOT NULL DEFAULT 0,
    created_at         TEXT NOT NULL DEFAULT (datetime('now'))
);
-- Real dedup guard for provider UIDs. A partial index so it applies only when a
-- UID is present (SQL NULLs are distinct, so an inline UNIQUE would silently
-- allow duplicate NULL-uid rows). NULL-uid messages dedup via the id primary key.
CREATE UNIQUE INDEX idx_messages_uid ON messages(account_id, mailbox_id, provider_uid)
    WHERE provider_uid IS NOT NULL;
-- Includes the id tiebreaker so OFFSET pagination is stable when sent_at ties.
CREATE INDEX idx_messages_list ON messages(account_id, mailbox_id, sent_at DESC, id);
CREATE INDEX idx_messages_rfc822 ON messages(rfc822_message_id);

CREATE TABLE message_addresses (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    kind       TEXT NOT NULL,   -- from | to | cc | bcc | reply-to
    name       TEXT,
    address    TEXT NOT NULL,
    ordinal    INTEGER NOT NULL
);
CREATE INDEX idx_message_addresses_msg ON message_addresses(message_id);

CREATE TABLE message_flags (
    message_id  TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    is_read     INTEGER NOT NULL DEFAULT 0,
    is_starred  INTEGER NOT NULL DEFAULT 0,
    is_answered INTEGER NOT NULL DEFAULT 0,
    is_draft    INTEGER NOT NULL DEFAULT 0,
    is_deleted  INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE message_bodies (
    message_id     TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    text_body      TEXT,
    sanitized_html TEXT,
    body_fetched_at TEXT
);
