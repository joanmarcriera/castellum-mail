-- 0028 (S33/S43/S44 already claim 0024 on their unmerged branches per the
-- roadmap's "Migration numbering" rule; this stage took the next free id —
-- renumbered from a provisional 0024, see the plan's Deviations list): smart
-- mailboxes (S41) — named, saved S39 search-grammar queries, run
-- cross-account through Database::search_messages_across_accounts exactly
-- like "All Inboxes" search (no second filter language). Counts are cached
-- here and refreshed by Database::recompute_smart_mailbox_counts /
-- recompute_all_smart_mailbox_counts (native/crates/castellum-db/src/
-- smart_mailboxes.rs) — on profile open, after each sync pass, and right
-- after a create/rename/query edit; never on a keystroke.
CREATE TABLE smart_mailboxes (
    id                 TEXT PRIMARY KEY,          -- uuid-v4 string
    name               TEXT NOT NULL,
    query              TEXT NOT NULL,              -- S39 search-grammar text
    sort_order         INTEGER NOT NULL DEFAULT 0, -- sidebar order, flat list
    symbol             TEXT,                       -- optional SF Symbol name
    cached_total       INTEGER NOT NULL DEFAULT 0,
    cached_unread      INTEGER NOT NULL DEFAULT 0,
    -- 1 when cached_total/cached_unread hit SMART_MAILBOX_COUNT_CAP and are
    -- therefore a lower bound, not an exact count (see smart_mailboxes.rs).
    counts_capped      INTEGER NOT NULL DEFAULT 0,
    counts_updated_at  INTEGER NOT NULL DEFAULT 0, -- epoch seconds, 0 = never
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL
);
CREATE UNIQUE INDEX idx_smart_mailboxes_name ON smart_mailboxes(name);
CREATE INDEX idx_smart_mailboxes_sort_order ON smart_mailboxes(sort_order);
