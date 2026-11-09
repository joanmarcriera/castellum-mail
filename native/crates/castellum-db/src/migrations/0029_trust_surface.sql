-- 0029: trust surface (S48) — SPF/DKIM/DMARC verdicts computed at sync time,
-- and a per-account, per-sender remote-content allow list.
--
-- message_trust mirrors the message_bodies/message_attachments precedent: a
-- 1:1 satellite table keyed by message_id, populated by
-- `new_message_from_raw` (native/crates/castellum-core/src/sync.rs) once per
-- hydrated message. Absence of a row (a message synced before this
-- migration) is read back as all-'none' by the repo layer, never as a
-- failure.
--
-- sender_remote_content_policy is new durable state: the per-message "Load
-- remote content" action can now optionally be remembered per (account,
-- sender), reviewable/revocable from Settings > General.
CREATE TABLE message_trust (
    message_id  TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    spf         TEXT NOT NULL DEFAULT 'none' CHECK (spf IN ('pass', 'fail', 'none')),
    dkim        TEXT NOT NULL DEFAULT 'none' CHECK (dkim IN ('pass', 'fail', 'none')),
    dmarc       TEXT NOT NULL DEFAULT 'none' CHECK (dmarc IN ('pass', 'fail', 'none')),
    authserv_id TEXT,
    computed_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE sender_remote_content_policy (
    account_id   TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- Lower-cased at write time by the repo layer (email addresses are
    -- case-insensitive in the local part by convention and always
    -- case-insensitive in the domain; storing pre-lowered avoids a COLLATE
    -- NOCASE index and keeps equality lookups a plain index scan).
    from_address TEXT NOT NULL,
    allowed      INTEGER NOT NULL DEFAULT 1 CHECK (allowed IN (0, 1)),
    updated_at   TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (account_id, from_address)
);
