-- 0033: thread action-item/date/amount extraction (S54). Model output for the
-- `extract_thread_items` AI feature is schema-validated (see
-- castellum-core/src/ai/defense.rs OutputSchema::Json + ai/extraction.rs)
-- before it is ever persisted here or shown — this table exists so the Swift
-- thread panel binds to typed rows instead of re-parsing the cached
-- ai_outputs JSON text on every render (see ai_outputs's own doc comment,
-- migration 0010).
--
-- Rows are replaced wholesale per thread each time a fresh extraction job
-- succeeds (castellum-core::ai::run's extract_thread_items success arm calls
-- `replace_thread_extractions`) — there is no per-item id contract to
-- reconcile across runs, so a stale item from a superseded run can never
-- linger next to a fresh one.
--
-- `message_id` is nullable: v1 extraction runs over the whole thread (see
-- `build_extract_thread_items`), so an item is not attributed to a single
-- message yet. `ON DELETE SET NULL` rather than CASCADE — losing track of
-- which message an item came from must never delete the item itself.
CREATE TABLE thread_extractions (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id        TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    message_id       TEXT REFERENCES messages(id) ON DELETE SET NULL,
    kind             TEXT NOT NULL CHECK (kind IN ('action_item', 'date', 'amount')),
    text             TEXT NOT NULL,
    structured_value TEXT,
    created_at       TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_thread_extractions_thread ON thread_extractions(thread_id, id);
