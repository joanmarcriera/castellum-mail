-- 0026: per-account identities/aliases (S47) — extra From addresses.
--
-- The account's own `accounts.email` is always the implicit default From
-- address and is NEVER itself a row here — this table holds only the
-- ADDITIONAL aliases a compose window's From picker offers alongside it
-- (see the S47 plan's Assumptions #7).
--
-- Also adds drafts.from_email/from_name: NULL = send as the account's own
-- identity (today's behaviour, unchanged); non-NULL = the alias chosen in
-- the From picker for THIS message, persisted (denormalized, like every
-- other drafts.* address field) so a delayed undo-send drain or a
-- crash-recovered outbox retry — both of which run long after the compose
-- window closed — still sends from the right address (see
-- castellum-core/src/send.rs's apply_send, S47 Task 4).
CREATE TABLE account_identities (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id   TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    email        TEXT NOT NULL,
    display_name TEXT,
    created_at   TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (account_id, email)
);
CREATE INDEX idx_account_identities_account ON account_identities(account_id);

ALTER TABLE drafts ADD COLUMN from_email TEXT;
ALTER TABLE drafts ADD COLUMN from_name TEXT;
