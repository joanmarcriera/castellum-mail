-- Durable, redacted sync diagnostics (support-bundle slice).
--
-- Records the last attempt, last success, and last (redacted) failure for
-- each account and mailbox so a support bundle and a future diagnostics UI
-- can show sync health without re-running a sync. Written only by the sync
-- engine at existing attempt/success/failure checkpoints; bundle generation
-- is strictly read-only. Error columns hold ONLY redaction-safe values
-- produced by castellum-core's redact module (a fixed kind tag, a fixed
-- phrase, a fixed phase name) - never raw server or provider error text.

-- Account-level state. Deliberately NO foreign key: the most important
-- failure to record is a connect/auth error on a first-ever sync, which
-- happens before any accounts row exists (sync creates the account row only
-- after connecting). Rows are keyed by the app-generated account id and may
-- outlive a deleted account; the bundle only reports rows for accounts it
-- can still see.
CREATE TABLE account_sync_state (
    account_id           TEXT PRIMARY KEY,
    last_attempt_at      TEXT,
    last_success_at      TEXT,
    last_error_kind      TEXT,
    last_error_redacted  TEXT,
    last_error_phase     TEXT,
    consecutive_failures INTEGER NOT NULL DEFAULT 0
);

-- Per-mailbox state. Mailbox rows are always upserted before an attempt is
-- recorded, so the cascading foreign keys are safe here.
CREATE TABLE mailbox_sync_state (
    account_id           TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    mailbox_id           TEXT NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE,
    last_attempt_at      TEXT,
    last_success_at      TEXT,
    last_error_kind      TEXT,
    last_error_redacted  TEXT,
    last_error_phase     TEXT,
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, mailbox_id)
);
