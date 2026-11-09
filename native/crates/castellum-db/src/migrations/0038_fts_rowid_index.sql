--: rowid-keyed FTS. `message_fts.message_id` was UNINDEXED
-- (project-docs/perf/2026-09-sync-baseline.md), so every DELETE/UPDATE
-- `... WHERE message_id = ?` scanned the whole index: O(rows) per call,
-- which made a hydrated-body re-index cost 8ms/message at 100k and a
-- mailbox purge (messages_after_delete_fts, firing once per deleted row)
-- O(n^2) overall.
--
-- Fix: a side table maps each message id to a small integer, and every
-- message_fts write/delete is now keyed by that integer via `WHERE rowid =
-- ?` (FTS5's own rowid IS indexed -- it's the primary key of the table's
-- shadow storage). Never `messages.rowid` itself: `messages` has a TEXT
-- primary key, so SQLite keeps a separate, ordinary implicit rowid for it
-- that VACUUM is free to renumber -- a value this table cannot borrow and
-- expect to stay stable.
CREATE TABLE message_fts_rowid (
    fts_rowid  INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id TEXT NOT NULL UNIQUE REFERENCES messages(id) ON DELETE CASCADE
);

-- Assign every existing message a mapping row, ordered by its (TEXT) id --
-- an arbitrary but deterministic and reproducible order, not insertion
-- order -- so the rebuild below is reproducible.
INSERT INTO message_fts_rowid (message_id)
SELECT id FROM messages ORDER BY id;

-- Rebuild message_fts from messages + message_bodies using the new mapping.
-- Same tokenizer, same three indexed columns, same derivation as
-- `upsert_message`'s indexed_subject/indexed_sender/body -- content and
-- ranking are unchanged, only the physical rowid each row lives at.
DROP TRIGGER messages_after_delete_fts;
DROP TABLE message_fts;

CREATE VIRTUAL TABLE message_fts USING fts5(
    message_id UNINDEXED,
    subject,
    sender,
    body,
    tokenize = 'unicode61'
);

INSERT INTO message_fts (rowid, message_id, subject, sender, body)
SELECT r.fts_rowid,
       m.id,
       COALESCE(m.subject, ''),
       TRIM(COALESCE(m.from_name, '') || ' ' || COALESCE(m.from_address, '')),
       COALESCE(b.text_body, '')
FROM messages m
JOIN message_fts_rowid r ON r.message_id = m.id
LEFT JOIN message_bodies b ON b.message_id = m.id;

-- Same propagation duty as before (FK cascades don't reach virtual tables),
-- now via the mapping's rowid instead of an UNINDEXED-column scan.
-- Deliberately BEFORE, not AFTER: message_fts_rowid's own
-- `ON DELETE CASCADE` on messages(id) removes its row for OLD.id as part of
-- the SAME statement, and a same-table FK cascade always runs before this
-- table's AFTER triggers fire (verified empirically -- an AFTER trigger's
-- lookup here would already see the mapping gone). BEFORE runs first, while
-- the mapping row is still present.
CREATE TRIGGER messages_before_delete_fts BEFORE DELETE ON messages BEGIN
    DELETE FROM message_fts WHERE rowid = (
        SELECT fts_rowid FROM message_fts_rowid WHERE message_id = OLD.id
    );
END;
