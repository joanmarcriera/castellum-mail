-- 0044: richer identities. An identity may carry a
-- Reply-To address and an automatic Bcc address (both applied by the core at
-- send time, so they hold for delayed undo-send drains and outbox retries), and
-- at most one identity per account may be flagged the account's DEFAULT (used
-- for new mail; the account's own address, which is never a row here, stays the
-- default when none is flagged). Existing identities: all NULL / 0 = unchanged.
ALTER TABLE account_identities ADD COLUMN reply_to TEXT;
ALTER TABLE account_identities ADD COLUMN bcc TEXT;
ALTER TABLE account_identities ADD COLUMN is_default INTEGER NOT NULL DEFAULT 0;
