-- 0041 (S42; written as 0039, renumbered to 0041 when merged over main's 0035-0040
-- -- the runner applies only ids above the MAX(id) watermark, see lib.rs):
-- tags = IMAP keywords (RFC 9051 §2.3.2) + local display metadata; mute = local,
-- conversation-scoped. See project-docs/plans/2026-09-25-tags-and-mute.md.
CREATE TABLE tag_definitions (
    keyword_key   TEXT PRIMARY KEY,           -- lower-cased keyword
    keyword       TEXT NOT NULL,              -- spelling written to servers
    display_name  TEXT NOT NULL,
    colour        TEXT NOT NULL DEFAULT 'grey', -- named palette, same convention as projects.color
    sort_order    INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);
CREATE UNIQUE INDEX idx_tag_definitions_name ON tag_definitions(display_name);

CREATE TABLE message_keywords (
    message_id  TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    keyword_key TEXT NOT NULL,
    keyword     TEXT NOT NULL,
    origin      TEXT NOT NULL DEFAULT 'server' CHECK (origin IN ('server','local')),
    PRIMARY KEY (message_id, keyword_key)
) WITHOUT ROWID;
CREATE INDEX idx_message_keywords_key ON message_keywords(keyword_key, message_id);

-- A conversation is muted when any member's anchor (RFC 5322 Message-ID, or
-- 'id:<message id>' without one) is here. No FK to threads: a thread's id is
-- `t:<root message id>`, so a root-changing merge or split gives the same
-- conversation a new id, and the rebuild recomputes threads.is_muted from here.
CREATE TABLE thread_mutes (
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    anchor     TEXT NOT NULL,
    muted_at   INTEGER NOT NULL,
    PRIMARY KEY (account_id, anchor)
) WITHOUT ROWID;

ALTER TABLE threads ADD COLUMN is_muted INTEGER NOT NULL DEFAULT 0;
