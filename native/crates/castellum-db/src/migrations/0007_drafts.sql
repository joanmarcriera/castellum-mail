-- 0007: compose drafts (compose/SMTP send slice, core).
--
-- A draft is composed locally, then queued for delivery: send_draft flips
-- status draft -> outbox and enqueues a `send` operation in the same
-- transaction; the SMTP send executor delivers it and marks it sent.
-- to/cc/bcc are JSON arrays of {name?, email} objects.
CREATE TABLE drafts (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id        TEXT NOT NULL,
    to_json           TEXT NOT NULL,
    cc_json           TEXT NOT NULL DEFAULT '[]',
    bcc_json          TEXT NOT NULL DEFAULT '[]',
    subject           TEXT NOT NULL DEFAULT '',
    body_text         TEXT NOT NULL DEFAULT '',
    in_reply_to       TEXT,
    references_header TEXT,
    status            TEXT NOT NULL DEFAULT 'draft'
                      CHECK(status IN ('draft','outbox','sent')),
    created_at        TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at        TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_drafts_account ON drafts(account_id, status, id);
