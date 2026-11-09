-- 0023: supporting indexes for the S34 unified (cross-account) message list.
--
-- `list_messages_by_role` (the tracker unified-inbox stage) queries across every
-- account by `mailboxes.role`, ordered globally by `messages.sent_at DESC,
-- id` (the same stable tiebreak `idx_messages_list` already uses per
-- account+mailbox). Neither existing index covers a cross-account role scan
-- or a global sort, so both would fall back to a full table scan once a
-- profile holds more than a handful of accounts/messages.
CREATE INDEX idx_mailboxes_role ON mailboxes(role);
CREATE INDEX idx_messages_sent_at ON messages(sent_at DESC, id);
