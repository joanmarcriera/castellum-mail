--: chunked descriptor fetch. A mailbox's first (or
-- from-scratch) descriptor import now proceeds in ~1000-UID chunks (see
-- `castellum_core::sync::sync_with_provider_scoped`) instead of one
-- unbounded `UID FETCH n:*`, so a large mailbox (a ~64 MiB / ~66 s real
-- Gmail INBOX first sync) checkpoints and shows progress every chunk
-- instead of buffering the whole response before any row is written.
--
-- descriptor_checkpoint_uid: the highest UID committed so far by an
-- in-progress chunked import, in the mailbox's CURRENT uid_validity space.
-- NULL means no chunked import is in progress -- either one never started,
-- or the mailbox's descriptor step for this pass has already finished:
-- `persist_cursor` always clears it back to NULL once that happens, whether
-- the fetch was chunked or (a provider without chunked support, or an
-- unknown UIDNEXT) took the old unbounded path.
--
-- Deliberately its OWN column, not a reuse of `uid_next`: `uid_next` only
-- ever holds the value from a FULLY completed pass, and the sync engine's
-- `same_uid_space` check treats "a cursor row exists" as "this mailbox has
-- been synced before" for the purpose of resuming instead of purging
-- ( / PR #175). Writing a mid-progress value into `uid_next`
-- would make a resumed-but-incomplete import look complete to that check
-- and to any future incremental sync reading it as "everything below this
-- UID is already imported."
ALTER TABLE sync_cursors ADD COLUMN descriptor_checkpoint_uid INTEGER;

-- The retry set (Opus review of PR #180): a chunked descriptor fetch that
-- isolated a UID as undecodable (sub-batch isolation in
-- `ImapProvider::fetch_descriptor_chunk`, CLAUDE.md gotcha 1's "never fail
-- the folder for one message") records it here instead of losing it --
-- without this, `descriptor_checkpoint_uid`/`uid_next` advance past a
-- skipped UID exactly as if it had been imported, and the message never
-- arrives even once a decoder bug is fixed. After a mailbox's import, the
-- sync engine re-fetches the rows whose `next_retry_at` has passed (at most
-- 20 UIDs per pass, on a separate 2-reconnect budget) and deletes any that
-- now decode (`castellum_core::sync`'s retry pass); `reason` is refreshed in
-- place on a repeat skip rather than inserting a duplicate row, so this is a
-- SET of currently-skipped UIDs, not an unbounded log -- `skipped_at` is when
-- it was LAST seen skipped. `reason` is always one of the fixed
-- classifications in `DescriptorSkipReason` -- NEVER the underlying
-- `SessionError`'s text, which may embed raw, untrusted server bytes (same
-- discipline as `castellum_core::redact`).
--
-- Backoff (review of PR #180, round 2, finding 3 -- a permanently bad UID
-- used to be retried on EVERY pass, two LOGINs each time, forever):
-- `attempts` = failed RETRIES so far (0 for a fresh skip); `next_retry_at`
-- = unix epoch seconds before which the row is not retried. A fresh skip is
-- due 1 hour later; each failed retry pushes it out 4 hours, then 24 hours,
-- then 7 days (the cap) -- `castellum_db::decode_skip_retry_delay_secs`. A
-- repeat skip during import never resets that backoff.
--
-- FKs cascade with the account/mailbox they belong to (`delete_account`,
-- and `reset_account`'s `DELETE FROM mailboxes`) exactly like `sync_cursors`
-- (migration 0002) -- a retry-set row for a mailbox/account that no longer
-- exists would retry forever for nothing.
CREATE TABLE message_decode_skips (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id   TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    mailbox_id   TEXT NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE,
    uid          INTEGER NOT NULL,
    uid_validity INTEGER NOT NULL,
    reason       TEXT NOT NULL,
    skipped_at   TEXT NOT NULL DEFAULT (datetime('now')),
    attempts     INTEGER NOT NULL DEFAULT 0,
    next_retry_at INTEGER NOT NULL,
    UNIQUE (account_id, mailbox_id, uid_validity, uid)
);
