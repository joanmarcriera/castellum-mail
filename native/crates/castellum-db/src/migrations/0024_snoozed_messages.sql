-- 0024: snooze and follow-up reminders (S43).
--
-- A message can be hidden from its normal mailbox/role list views until a
-- chosen time, then reappear automatically. Unlike undo-send's ALTER-TABLE-
-- on-an-existing-row style (migration 0020), snooze is naturally a side
-- table: at most one open snooze exists per message, looked up by id, with
-- its own due-time index independent of `messages`' own columns.
--
-- SQLite, not an in-memory timer, is the source of truth: the due-check
-- (Database::unsnooze_due) is a plain time comparison against this table on
-- every launch and on a periodic sweep, so a message is never resurfaced (or
-- kept hidden) unpredictably after an ambiguous crash (TASKS.md's
-- time-based-triage completion criterion).
--
-- A message in ANY mailbox (Inbox, Sent, ...) can be snoozed — snoozing a
-- Sent-mailbox message as a manual follow-up reminder needs no extra schema.
-- Automatic no-reply detection (auto-creating or auto-cancelling a reminder
-- based on whether a reply arrived) is out of scope for S43 — see the plan's
-- Assumptions §9.
CREATE TABLE snoozed_messages (
    message_id          TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    account_id           TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    snoozed_until_epoch  INTEGER NOT NULL,
    created_at           TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Database::unsnooze_due scans/orders by this column; Database::
-- list_snoozed_messages (the Snoozed sidebar list) orders by it too (soonest
-- due first). list_message_headers/list_messages_by_role LEFT JOIN this
-- table to exclude a not-yet-due row, so it is also on the hot path of every
-- normal mailbox list.
CREATE INDEX idx_snoozed_messages_due ON snoozed_messages(snoozed_until_epoch);
