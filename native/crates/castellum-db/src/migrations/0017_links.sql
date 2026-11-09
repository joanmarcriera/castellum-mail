-- 0017: shared evidence-backed link model.
--
-- One generic `links` table becomes the single source of truth for
-- cross-entity relationships: typed endpoints (project / message / thread /
-- contact / future work_item), a relation kind, an origin ('manual',
-- 'derived', or a named source), a provenance reference that answers "why is
-- this connected?" (entity ids only — NEVER message bodies or copied text),
-- optional confidence for derived links, a user-owned note that machine
-- refreshes can never overwrite, and created/updated timestamps.
--
-- project_emails / project_contacts are backfilled and DROPPED — the legacy
-- project-link APIs keep their exact signatures and JSON shapes on top of
-- `links` (stored origin 'derived' renders as legacy "auto" in the
-- ProjectContactLink DTO). Deliberately NOT moved: contact_edges (a derived
-- co-occurrence graph recomputed atomically on refresh — not a user-ownable
-- relationship row) and contact_enrichment (per-contact metadata, not a
-- relation between two entities).
--
-- Polymorphic endpoints cannot carry SQL foreign keys, so per-entity delete
-- triggers reproduce the old ON DELETE CASCADE behaviour. Threads are
-- virtual (messages carry a thread id; there is no threads-owned lifecycle),
-- so thread links live until explicitly unlinked.

CREATE TABLE links (
    id          INTEGER PRIMARY KEY,
    src_type    TEXT NOT NULL CHECK (src_type IN ('project','message','thread','contact','work_item')),
    src_id      TEXT NOT NULL CHECK (length(src_id) BETWEEN 1 AND 512),
    dst_type    TEXT NOT NULL CHECK (dst_type IN ('project','message','thread','contact','work_item')),
    dst_id      TEXT NOT NULL CHECK (length(dst_id) BETWEEN 1 AND 512),
    relation    TEXT NOT NULL CHECK (length(relation) BETWEEN 1 AND 64),
    origin      TEXT NOT NULL CHECK (length(origin) BETWEEN 1 AND 64),
    provenance  TEXT CHECK (provenance IS NULL OR length(provenance) <= 512),
    confidence  REAL CHECK (confidence IS NULL OR (confidence >= 0.0 AND confidence <= 1.0)),
    note        TEXT,   -- user-owned; only the explicit note API writes this
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    UNIQUE (src_type, src_id, dst_type, dst_id, relation)
);
CREATE INDEX idx_links_src ON links(src_type, src_id);
CREATE INDEX idx_links_dst ON links(dst_type, dst_id);

-- Backfill: project -> message ("includes"); manual by construction.
INSERT INTO links (src_type, src_id, dst_type, dst_id, relation, origin,
                   provenance, confidence, note, created_at, updated_at)
SELECT 'project', project_id, 'message', message_id, 'includes', 'manual',
       NULL, NULL, NULL, linked_at, linked_at
FROM project_emails;

-- Backfill: project -> contact ("involves"). Legacy 'auto' rows become
-- 'derived' with generic participant provenance (the evidencing message id
-- was not recorded pre-0017; new derived links carry it).
INSERT INTO links (src_type, src_id, dst_type, dst_id, relation, origin,
                   provenance, confidence, note, created_at, updated_at)
SELECT 'project', project_id, 'contact', CAST(contact_id AS TEXT), 'involves',
       CASE origin WHEN 'auto' THEN 'derived' ELSE 'manual' END,
       CASE origin WHEN 'auto' THEN 'participant' ELSE NULL END,
       NULL, NULL, linked_at, linked_at
FROM project_contacts;

DROP TABLE project_emails;
DROP TABLE project_contacts;

-- Cascade triggers: polymorphic endpoints can't use FKs.
CREATE TRIGGER links_project_delete AFTER DELETE ON projects BEGIN
    DELETE FROM links WHERE (src_type = 'project' AND src_id = OLD.id)
                         OR (dst_type = 'project' AND dst_id = OLD.id);
END;
CREATE TRIGGER links_message_delete AFTER DELETE ON messages BEGIN
    DELETE FROM links WHERE (src_type = 'message' AND src_id = OLD.id)
                         OR (dst_type = 'message' AND dst_id = OLD.id);
END;
CREATE TRIGGER links_contact_delete AFTER DELETE ON contacts BEGIN
    DELETE FROM links WHERE (src_type = 'contact' AND src_id = CAST(OLD.id AS TEXT))
                         OR (dst_type = 'contact' AND dst_id = CAST(OLD.id AS TEXT));
END;
