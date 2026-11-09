-- 0020: undo-send window (S25).
--
-- A `send` operation can carry a durable earliest-send time: the sender skips
-- it (never delivers) until `not_before_epoch` has passed, giving the compose
-- UI a crash-safe grace period during which the user can undo. NULL preserves
-- today's behaviour (due immediately) for every operation this column doesn't
-- apply to — only a delayed `send` op ever sets it.
ALTER TABLE operations ADD COLUMN not_before_epoch INTEGER;

-- No CHECK constraint exists on `operations.status` (see migration 0005), so
-- introducing the `held` status needs no further schema change: the crash-
-- safety startup pass (Database::reconcile_undo_sends, DECISIONS.md) sets it
-- on a queued `send` op whose undo window cannot be proven to have stayed
-- open under a continuously-live app. `due_for_attempt` treats any status it
-- doesn't recognise as not-due, so `held` is inert to the normal executor by
-- construction; only `Database::send_now`/`Database::undo_send` move it on.
--
-- `app_meta` (migration 1) gains one row, key 'last_clean_drain_epoch': the
-- last epoch second at which a send drain call started (Database::
-- touch_last_clean_drain), read back by the reconciliation pass above.
