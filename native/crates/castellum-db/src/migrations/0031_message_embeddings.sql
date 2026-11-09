-- 0031: message embeddings (S52). One vector per message, computed locally by
-- the existing AI job queue (feature `embed_message`, background priority —
-- see castellum-db/src/ai.rs `AI_PRIORITY_BACKGROUND`) and stored as a
-- little-endian f32 BLOB. No native vector extension (e.g. sqlite-vec) is in
-- the workspace yet; similarity search over these vectors is deferred to a
-- later slice's budget. Re-embedding (a re-hydrated body, or a newer model)
-- replaces the row in place, keyed by message_id — mirrors the
-- one-row-per-message upsert shape of message_trust (migration 0029).
CREATE TABLE message_embeddings (
    message_id  TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    model_name  TEXT NOT NULL,
    dimension   INTEGER NOT NULL CHECK (dimension > 0),
    vector      BLOB NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
