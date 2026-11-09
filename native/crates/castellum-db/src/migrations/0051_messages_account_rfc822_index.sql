-- 0051 (R2, rules dedupe): the Gmail copy-dedupe window in rule_candidates
-- partitions messages by (account_id, rfc822_message_id); without this index
-- every Run Now batch re-sorts the whole messages table. Index only: no data
-- change, and which messages a rule matches is untouched (dedupe stays
-- account-agnostic).
CREATE INDEX idx_messages_account_rfc822 ON messages(account_id, rfc822_message_id);
