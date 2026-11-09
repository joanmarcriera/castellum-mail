-- 0046: suggestions you accept. Castellum PROPOSES, the
-- user decides; nothing is ever created from mail automatically (DECISIONS.md,
-- 2026-10-01 entry on waiting-for-reply). A proposal is a row here, NOT a
-- work_items row, until the user accepts it.
--
--   kind        work_item | waiting_for_reply | state_change
--   status      proposed | accepted | dismissed (decided_at set on the last two)
--   payload_json  the PROPOSAL: {title?, due_at?, to_state?} - a proposed title
--               is user-visible suggestion text, never a copy of mail content
--   evidence    IDS ONLY (links-model rule, DECISIONS.md "Link-model
--               integrity"): message_id, thread_id, extraction_id,
--               rfc822_message_id. No body, snippet or subject is stored, and
--               none of these is a foreign key: the evidence may be purged
--               (extraction rows are replaced wholesale per run, threads are
--               virtual) without losing the user's decision.
--   target_work_item_id   state_change only: the item to move. CASCADE: a
--               state change for a deleted item is meaningless.
--   created_work_item_id  set on accept (the item this suggestion became);
--               SET NULL if the user later deletes that item.
--   origin      heuristic | ai_extraction
--   confidence  optional 0..1
--   dedupe_key  UNIQUE across ALL statuses: a dismissed suggestion's key can
--               never produce a new proposal.
CREATE TABLE suggestions (
    id                    TEXT PRIMARY KEY,
    kind                  TEXT NOT NULL
        CHECK (kind IN ('work_item', 'waiting_for_reply', 'state_change')),
    status                TEXT NOT NULL DEFAULT 'proposed'
        CHECK (status IN ('proposed', 'accepted', 'dismissed')),
    payload_json          TEXT NOT NULL,
    message_id            TEXT,
    thread_id             TEXT,
    extraction_id         INTEGER,
    rfc822_message_id     TEXT,
    target_work_item_id   TEXT REFERENCES work_items(id) ON DELETE CASCADE,
    created_work_item_id  TEXT REFERENCES work_items(id) ON DELETE SET NULL,
    origin                TEXT NOT NULL CHECK (origin IN ('heuristic', 'ai_extraction')),
    confidence            REAL CHECK (confidence IS NULL OR (confidence >= 0 AND confidence <= 1)),
    dedupe_key            TEXT NOT NULL UNIQUE,
    created_at            INTEGER NOT NULL,
    decided_at            INTEGER
);
CREATE INDEX idx_suggestions_status ON suggestions(status, created_at DESC);
CREATE INDEX idx_suggestions_thread ON suggestions(thread_id);
