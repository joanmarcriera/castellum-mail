-- 0034: declarative filters — backs smart mailboxes and rules (S40).
--
-- One row per saved filter. `group_json` holds the serialized FilterGroup
-- (see castellum-models::filter). Rules additionally set trigger_kind and
-- actions_json; smart mailboxes leave both NULL/'[]'.

CREATE TABLE saved_filters (
    id           TEXT PRIMARY KEY,
    account_id   TEXT REFERENCES accounts(id) ON DELETE CASCADE,  -- NULL = all accounts
    name         TEXT NOT NULL,
    kind         TEXT NOT NULL CHECK (kind IN ('smart_mailbox', 'rule')),
    group_json   TEXT NOT NULL,     -- serialized FilterGroup
    sort_field   TEXT,
    sort_desc    INTEGER NOT NULL DEFAULT 1,
    trigger_kind TEXT CHECK (trigger_kind IS NULL OR trigger_kind IN ('on_sync', 'manual')),
    -- serialized Vec<RuleAction>; defaults to '[]' (not NULL) so a
    -- smart-mailbox row can be queried with `actions_json != '[]'`
    -- uniformly without a NULL check (S40 plan Task 2's compiler-side
    -- simplification — not in the memo's literal SQL).
    actions_json TEXT NOT NULL DEFAULT '[]',
    created_at   TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at   TEXT NOT NULL DEFAULT (datetime('now')),
    CHECK (
        (kind = 'smart_mailbox' AND trigger_kind IS NULL)
        OR (kind = 'rule' AND trigger_kind IS NOT NULL)
    )
);
CREATE INDEX idx_saved_filters_account ON saved_filters(account_id);

-- Conditional on S0 (memo §7): per-message Gist topic, if/when Marc approves
-- storing it. Not created by this migration; sketched here for reference
-- only. A future migration would add:
--   CREATE TABLE message_gist_topics (
--       message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
--       topic      TEXT NOT NULL,
--       tagged_at  TEXT NOT NULL DEFAULT (datetime('now'))
--   );
