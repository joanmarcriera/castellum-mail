-- 0047: saved Active Work views.
--
-- A view is a name plus a work-item filter in the AW1 filter schema
-- (`WorkItemFilter`, validated on every write; the one evaluator is
-- `list_work_items_filtered`). Built-in views (Today, Needs reply, Waiting on
-- others, Someday) are seeded by Rust on first use with fixed `builtin:*` ids,
-- idempotently; they can be reordered and hidden but never deleted, renamed
-- or re-filtered. Id 46 is reserved by another stage on the same train.
--
--   filter    JSON text, canonicalised on write
--   position  display order (dense, 0-based, rewritten by reorder)
--   built_in  1 for the seeded views
--   hidden    1 = left out of the sidebar and of the counts
CREATE TABLE saved_views (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL CHECK (length(trim(name)) BETWEEN 1 AND 120),
    filter      TEXT NOT NULL,
    position    INTEGER NOT NULL DEFAULT 0,
    built_in    INTEGER NOT NULL DEFAULT 0 CHECK (built_in IN (0, 1)),
    hidden      INTEGER NOT NULL DEFAULT 0 CHECK (hidden IN (0, 1)),
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
CREATE UNIQUE INDEX idx_saved_views_name ON saved_views(lower(name));
CREATE INDEX idx_saved_views_position ON saved_views(position, id);
