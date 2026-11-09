-- 0022: content-addressed local cache for downloaded incoming attachments
-- (S32). Keyed by (message_id, ordinal) -> the sha256 hex of the downloaded
-- bytes; the bytes themselves live as one file per hash in the profile's
-- attachments-cache directory (see castellum_core::attachments::cache_dir),
-- so two attachments with identical bytes (e.g. the same PDF forwarded to
-- two accounts) are stored once. Rows cascade with their message; a cached
-- file on disk is only deleted once no row references its hash any more
-- (see castellum_core::attachments::evict_if_over_budget).
CREATE TABLE attachment_cache (
    message_id          TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    ordinal              INTEGER NOT NULL CHECK (ordinal >= 0),
    content_hash         TEXT NOT NULL,
    size_bytes           INTEGER NOT NULL CHECK (size_bytes >= 0),
    last_accessed_epoch  INTEGER NOT NULL,
    PRIMARY KEY (message_id, ordinal)
);
CREATE INDEX idx_attachment_cache_hash ON attachment_cache(content_hash);
