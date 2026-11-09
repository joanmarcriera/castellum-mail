-- 0045: Active Work dates, waiting metadata and an
-- optional delegate on work items, and work items that belong to no project.
--
--   due_at                   nullable epoch seconds (same representation as
--                            created_at/updated_at)
--   waiting_since            set by the core on entering 'waiting', cleared on
--                            leaving it
--   completed_at             set by the core on entering 'done', cleared on
--                            leaving it
--   delegated_to_contact_id  optional contact for 'delegated' items; the row
--                            is kept and the column becomes NULL when the
--                            contact is deleted (FK ON DELETE SET NULL)
--   project_id               now NULLABLE: NULL = an unassigned item (the
--                            "unassigned" list filter). Deleting a project
--                            still cascades the items it owns.
--
-- SQLite cannot drop NOT NULL, and a column carrying an FK cannot later be
-- dropped, so the table is rebuilt in place (ids, positions and timestamps
-- are copied unchanged, so every `links` row stays valid). Title and notes
-- are copied verbatim: user-owned text is never touched by a migration.
CREATE TABLE work_items_v45 (
    id          TEXT PRIMARY KEY,                -- uuid-v4 string
    project_id  TEXT REFERENCES projects(id) ON DELETE CASCADE,
    title       TEXT NOT NULL,                   -- user-owned
    notes       TEXT NOT NULL DEFAULT '',        -- user-owned
    state       TEXT NOT NULL DEFAULT 'needs_reply'
                CHECK (state IN ('needs_reply','waiting','delegated',
                                 'blocked','done','someday')),
    position    INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    due_at                  INTEGER,
    waiting_since           INTEGER,
    completed_at            INTEGER,
    delegated_to_contact_id INTEGER REFERENCES contacts(id) ON DELETE SET NULL
);
INSERT INTO work_items_v45
    (id, project_id, title, notes, state, position, created_at, updated_at)
SELECT id, project_id, title, notes, state, position, created_at, updated_at
FROM work_items;
DROP TABLE work_items;
ALTER TABLE work_items_v45 RENAME TO work_items;

CREATE INDEX idx_work_items_project ON work_items(project_id, state, position);
-- Serves the filtered list's stable order: due_at ascending, NULLs last, then
-- position, then id.
CREATE INDEX idx_work_items_due ON work_items((due_at IS NULL), due_at, position, id);
CREATE INDEX idx_work_items_delegate ON work_items(delegated_to_contact_id)
    WHERE delegated_to_contact_id IS NOT NULL;

-- The cascade trigger died with the old table (see migration 0018).
CREATE TRIGGER links_work_item_delete AFTER DELETE ON work_items BEGIN
    DELETE FROM links WHERE (src_type = 'work_item' AND src_id = OLD.id)
                         OR (dst_type = 'work_item' AND dst_id = OLD.id);
END;
