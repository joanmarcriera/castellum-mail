-- 0056 (bug hunt #50): arrivals waiting for their body before the on_sync rules may judge them.
-- A sync pass hydrates at most 25 bodies per mailbox, newest first, but runs the rules once over
-- every arrival. A rule with a Body or Attachment condition reads message_fts / message_attachments,
-- which are empty until the body arrives, so arrivals beyond the cap silently missed such rules
-- forever (later hydration never re-queues them). While an enabled on_sync rule needs bodies, the
-- not-yet-hydrated arrivals are parked here instead, and every place a body lands (sync pass,
-- background hydration, body-on-open) runs the rules for the parked messages that now have one.
-- Rows expire after 7 days (a message the hydrator skips, e.g. oversized, must not wait forever)
-- and vanish with their message.
CREATE TABLE pending_rules (
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (account_id, message_id)
);
