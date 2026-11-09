-- Durable operation queue (write path, slice 7a). Every provider-affecting
-- mutation is recorded here and applied optimistically to the local DB in the
-- same transaction; a later executor replays queued ops to the provider,
-- idempotently. FIFO per account by id.
CREATE TABLE operations (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id   TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind         TEXT NOT NULL,              -- e.g. "mark_read"
    payload_json TEXT NOT NULL,
    status       TEXT NOT NULL DEFAULT 'queued', -- queued|running|succeeded|failed|failed_permanent|cancelled
    attempts     INTEGER NOT NULL DEFAULT 0,
    last_error   TEXT,
    created_at   TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at   TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_operations_pending ON operations(account_id, status, id);
