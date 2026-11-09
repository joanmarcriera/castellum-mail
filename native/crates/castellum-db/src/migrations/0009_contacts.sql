-- 0009: contacts + co-occurrence edges (contacts slice 8a).
--
-- contacts is aggregated from message_addresses by refresh_contact_aggregates:
-- derived columns (derived_name, is_self, first/last_seen, counts) are
-- recomputed on every refresh; user-owned columns (display_name, notes,
-- is_hidden, is_user_created) are NEVER touched by refresh.
-- contact_edges is the people co-occurrence graph, rebuilt from scratch on
-- every refresh (canonical contact_a < contact_b; self/bcc excluded).
--
-- NOTE: migration id 8 is reserved for a parallel AI-slice PR; whichever
-- merges second renumbers (greenfield DB).
CREATE TABLE contacts (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    email         TEXT NOT NULL UNIQUE,          -- normalized lowercase/trimmed
    derived_name  TEXT,                          -- best-pick from headers (recomputed by refresh)
    display_name  TEXT,                          -- user override; NEVER touched by refresh
    notes         TEXT NOT NULL DEFAULT '',
    is_hidden     INTEGER NOT NULL DEFAULT 0,
    is_self       INTEGER NOT NULL DEFAULT 0,    -- matches an accounts.email (recomputed)
    is_user_created INTEGER NOT NULL DEFAULT 0,
    first_seen_at INTEGER,
    last_seen_at  INTEGER,
    inbound_count  INTEGER NOT NULL DEFAULT 0,
    outbound_count INTEGER NOT NULL DEFAULT 0,
    updated_at    TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_contacts_last_seen ON contacts(last_seen_at DESC);

CREATE TABLE contact_edges (
    contact_a INTEGER NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
    contact_b INTEGER NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
    weight REAL NOT NULL,
    message_count INTEGER NOT NULL,
    last_seen_at INTEGER,
    PRIMARY KEY (contact_a, contact_b),
    CHECK (contact_a < contact_b)
);
CREATE INDEX idx_contact_edges_b ON contact_edges(contact_b);
