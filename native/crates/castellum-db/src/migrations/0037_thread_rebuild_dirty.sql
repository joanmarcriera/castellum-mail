-- S64 review fix: sync.rs now skips `rebuild_threads` when a pass fetched
-- nothing, reconciled no flags and purged no vanished messages (0036's perf
-- win). But a LOCAL mutation that already replayed to the server -- mark_read,
-- flag, delete/move (enqueue_move), mark_all_read, empty_trash -- changes
-- threads.unread_count/message_count without the server changing anything,
-- so the next sync's "nothing changed remotely" test is true while the
-- projection is stale. This table is the per-account marker that closes that
-- gap: every local mutation that can move a thread aggregate sets `dirty` to
-- 1 in the SAME transaction as the mutation; `rebuild_threads` clears it back
-- to 0 in the same transaction as the rebuild it performs. Sync's skip
-- decision becomes "no remote change AND not dirty".
--
-- FK to `accounts` with ON DELETE CASCADE: a row here is written inside the
-- SAME transaction as a message mutation, which itself requires an existing
-- account row (the mutated message already belongs to one), so the FK
-- enforces nothing new at write time -- but it is still declared so deleting
-- an account cleans up its marker row instead of leaving it orphaned.
CREATE TABLE thread_rebuild_state (
    account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    dirty      INTEGER NOT NULL DEFAULT 0
);
