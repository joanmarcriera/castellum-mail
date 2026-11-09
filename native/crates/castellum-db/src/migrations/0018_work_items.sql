-- 0018: explicit work items (Active Work, backend slice).
--
-- A work item is a user-owned unit of work inside a project: title and notes
-- are NEVER machine-written, state is one of the six Active Work states, and
-- ordering is deterministic (state, position, created_at, id). Work items are
-- NEVER created automatically from mail — creation is an explicit user act.
-- Relationships to messages/threads/contacts go through the shared `links`
-- table (endpoint kind 'work_item', migration 0017), so this migration adds
-- the missing cascade trigger for that endpoint kind.

CREATE TABLE work_items (
    id          TEXT PRIMARY KEY,                -- uuid-v4 string
    project_id  TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    title       TEXT NOT NULL,                   -- user-owned
    notes       TEXT NOT NULL DEFAULT '',        -- user-owned
    state       TEXT NOT NULL DEFAULT 'needs_reply'
                CHECK (state IN ('needs_reply','waiting','delegated',
                                 'blocked','done','someday')),
    position    INTEGER NOT NULL DEFAULT 0,      -- sort within a state lane
    created_at  INTEGER NOT NULL,                -- epoch seconds
    updated_at  INTEGER NOT NULL
);
CREATE INDEX idx_work_items_project ON work_items(project_id, state, position);

CREATE TRIGGER links_work_item_delete AFTER DELETE ON work_items BEGIN
    DELETE FROM links WHERE (src_type = 'work_item' AND src_id = OLD.id)
                         OR (dst_type = 'work_item' AND dst_id = OLD.id);
END;
