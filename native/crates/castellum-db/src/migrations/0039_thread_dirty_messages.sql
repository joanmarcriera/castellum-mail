--: incremental thread rebuild. `rebuild_threads` (0037) always
-- recomputes the WHOLE account's thread projection in one transaction --
-- ~0.8-1.3s at 100k messages while holding the DB mutex, the documented
-- cause of the read-path p95/max miss during sync
-- (project-docs/perf/read-path-budgets.md). 0037's `thread_rebuild_state`
-- only tracked a per-account boolean ("something is stale"), which is enough
-- to decide WHETHER to rebuild but not WHAT changed -- so any dirty account
-- still paid a full rebuild. This migration adds the "what changed" half.
--
-- `thread_dirty_messages` replaces the boolean with the actual message ids a
-- write touched (`mark_threads_dirty`, `castellum-db/src/repo.rs`):
-- `rebuild_threads_incremental` expands each one to its connected component
-- under the SAME graph `rebuild_threads` unions over (rfc822 duplicates +
-- References) and recomputes only the threads that touches, in bounded
-- chunks. `rebuild_threads` (full) still clears the whole set, exactly as it
-- cleared `thread_rebuild_state.dirty` before.
--
-- `gen` (PR #181 review, finding 3): a write to a message that is ALREADY
-- dirty bumps it (`ON CONFLICT ... SET gen = gen + 1`) instead of being
-- ignored. The incremental rebuild snapshots (message_id, gen) before it
-- starts and clears a row only while its gen still matches, so a write that
-- lands between the snapshot and the chunk that processes it leaves the row
-- for the next pass -- it can never be cleared unseen.
CREATE TABLE thread_dirty_messages (
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    message_id TEXT NOT NULL,
    gen        INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, message_id)
);

-- A whole-account escape hatch for mutations too broad to name individual
-- messages -- a mailbox purge (UIDVALIDITY reset, the legacy empty_trash
-- recovery path) or the first rebuild an account has ever seen. Set instead
-- of populating `thread_dirty_messages`; `rebuild_threads_incremental`
-- always takes the full-rebuild fallback while it is 1, and both rebuild
-- paths clear it back to 0.
ALTER TABLE thread_rebuild_state ADD COLUMN force_full INTEGER NOT NULL DEFAULT 0;

-- An account already marked dirty by 0037's boolean has no per-message rows
-- to say WHAT is stale, so its next rebuild must be a full one: carry the
-- marker over as force_full. Without this, an upgraded profile's pending
-- rebuild would read as "nothing to do" forever.
UPDATE thread_rebuild_state SET force_full = 1 WHERE dirty = 1;
