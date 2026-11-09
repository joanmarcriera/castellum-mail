-- 0050 (Active Work provenance): an explicit "this work item came from X" record,
-- separate from the user-owned work_items.notes (which is never rewritten) and from
-- the shareable links table. Written when a suggestion is accepted and when a work
-- item is linked to a message; 'manual' marks a deliberate user attribution.
-- Independent of 0049 (another track): touches only work_items(id).
CREATE TABLE work_item_provenance (
    work_item_id TEXT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    source_kind  TEXT NOT NULL CHECK (source_kind IN ('message','suggestion','manual')),
    source_id    TEXT NOT NULL,             -- message id / suggestion id / '' for manual
    created_at   INTEGER NOT NULL,
    PRIMARY KEY (work_item_id, source_kind, source_id)
) WITHOUT ROWID;
CREATE INDEX idx_work_item_provenance_source
    ON work_item_provenance(source_kind, source_id);
