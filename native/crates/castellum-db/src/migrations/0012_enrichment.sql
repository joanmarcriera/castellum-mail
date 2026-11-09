-- 0012: contact enrichment fields (slice 8d).
--
-- Stores enrichment data for contacts from external sources (manual paste,
-- future HTTP adapters). One row per (contact_id, source, field) triple;
-- re-running an adapter upserts values in place (UNIQUE constraint).
--
-- id 11 is reserved for PR #50 (projects); this migration follows with id 12.
-- ON DELETE CASCADE keeps the table tidy when a contact is removed.

CREATE TABLE contact_enrichment (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    contact_id INTEGER NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
    source     TEXT NOT NULL,
    -- 'manual' | future adapter ids (e.g. 'linkedin_api', 'clearbit')
    field      TEXT NOT NULL,
    -- 'title' | 'company' | 'location' | 'linkedin_url' | 'summary' | free-form keys
    value      TEXT NOT NULL,
    fetched_at INTEGER NOT NULL,
    -- Unix epoch seconds when the field was fetched/pasted
    provenance TEXT,
    -- human-readable source hint, e.g. 'pasted 2026-07-03', or a URL
    -- The UNIQUE constraint also serves as the (contact_id, source, field)
    -- lookup index — no separate CREATE INDEX needed.
    UNIQUE (contact_id, source, field)
);
