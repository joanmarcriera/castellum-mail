-- 0030: outgoing compose attachments (S33). Metadata lives in
-- draft_attachments; bytes live in draft_attachment_blobs keyed by
-- attachment id, mirroring operation_blobs (migration 0015) so listing a
-- draft's attachments never loads payload bytes. Both cascade with their
-- draft; a blob cascades with its own attachment row. is_inline/content_id
-- mirror message_attachments (migration 0021) so a forwarded original
-- inline image keeps its cid: reference.
CREATE TABLE draft_attachments (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    draft_id      INTEGER NOT NULL REFERENCES drafts(id) ON DELETE CASCADE,
    ordinal       INTEGER NOT NULL CHECK (ordinal >= 0),
    filename      TEXT NOT NULL,
    content_type  TEXT NOT NULL,
    size_bytes    INTEGER NOT NULL CHECK (size_bytes >= 0),
    is_inline     INTEGER NOT NULL DEFAULT 0,
    content_id    TEXT
);
CREATE INDEX idx_draft_attachments_draft ON draft_attachments(draft_id, ordinal);

CREATE TABLE draft_attachment_blobs (
    attachment_id INTEGER PRIMARY KEY REFERENCES draft_attachments(id) ON DELETE CASCADE,
    bytes         BLOB NOT NULL
);
