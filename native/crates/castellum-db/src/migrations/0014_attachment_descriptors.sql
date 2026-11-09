-- Incoming attachment metadata discovered while parsing a hydrated full body.
-- Payload bytes and local cache paths intentionally remain out of this table.

CREATE TABLE message_attachments (
    message_id   TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    ordinal      INTEGER NOT NULL CHECK (ordinal >= 0),
    filename     TEXT,
    content_type TEXT,
    size_bytes   INTEGER CHECK (size_bytes IS NULL OR size_bytes >= 0),
    PRIMARY KEY (message_id, ordinal)
);
