-- 0057 (MM-1c / #2710 slice 1): selected message headers, so a smart mailbox can
-- match "any header" (List-Id, X-Mailer, X-GitHub-Reason, Reply-To, ...).
-- Nothing held header data before (raw RFC822 is deliberately never stored).
-- Normalised satellite tables, not a JSON blob: a condition touches only the
-- rows for one header name through idx_message_headers_name.
-- Values are mail content (local-only, never exported, never logged). The
-- denylist, caps and RFC 2047 decoding live in castellum-models/castellum-mail.
-- No ALTER TABLE; rollback hint: DROP TABLE message_header_capture;
-- DROP TABLE message_headers; DROP TABLE header_names;

-- Dictionary: one row per distinct lower-cased header name.
CREATE TABLE header_names (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);

-- One row per stored header occurrence (ordinal = 0-based document order per name).
CREATE TABLE message_headers (
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    name_id    INTEGER NOT NULL REFERENCES header_names(id),
    ordinal    INTEGER NOT NULL,
    value      TEXT NOT NULL,
    PRIMARY KEY (message_id, name_id, ordinal)
) WITHOUT ROWID;
CREATE INDEX idx_message_headers_name ON message_headers(name_id, message_id);

-- "Headers were looked at for this message", even if none were kept. Absence of
-- a row means unknown (the message_trust rule), never an error. partial = 1 when
-- some headers were dropped because the name dictionary was full (a hostile
-- sender can fill it); "header is absent" must treat such a message as UNKNOWN.
CREATE TABLE message_header_capture (
    message_id  TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    source      TEXT NOT NULL CHECK (source IN ('body', 'header_fetch')),
    captured_at TEXT NOT NULL DEFAULT (datetime('now')),
    partial     INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;

-- A dictionary name no stored header uses any more is removed (on replace and on
-- message delete, which cascades here), so the dictionary cap cannot stay
-- filled by names whose mail is gone.
CREATE TRIGGER message_headers_after_delete_name AFTER DELETE ON message_headers
BEGIN
    DELETE FROM header_names WHERE id = OLD.name_id
        AND NOT EXISTS (SELECT 1 FROM message_headers WHERE name_id = OLD.name_id);
END;
