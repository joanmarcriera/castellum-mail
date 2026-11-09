-- 0049 (Active Work): append-only per-project activity + decision timeline.
-- Rows hold a short summary and reference ids only (never message bodies).
-- Append-only: rows cannot be updated, and cannot be deleted except by the
-- cascade of deleting the project itself.
CREATE TABLE project_timeline (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id  TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    kind        TEXT NOT NULL CHECK (kind IN ('activity','decision','work_item_change')),
    summary     TEXT NOT NULL CHECK (length(summary) > 0),
    ref_type    TEXT CHECK (ref_type IS NULL OR ref_type IN ('message','thread','contact','work_item')),
    ref_id      TEXT,
    created_at  INTEGER NOT NULL
);
CREATE INDEX idx_project_timeline_project ON project_timeline(project_id, created_at DESC, id DESC);

CREATE TRIGGER project_timeline_no_update BEFORE UPDATE ON project_timeline BEGIN
    SELECT RAISE(ABORT, 'project_timeline is append-only');
END;
CREATE TRIGGER project_timeline_no_delete BEFORE DELETE ON project_timeline
WHEN EXISTS (SELECT 1 FROM projects WHERE id = OLD.project_id) BEGIN
    SELECT RAISE(ABORT, 'project_timeline is append-only');
END;
