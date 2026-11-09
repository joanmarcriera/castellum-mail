-- Mailbox discovery policy metadata.
--
-- LIST/LSUB discovery needs to preserve provider hierarchy and subscription
-- state so the UI and sync policy can distinguish "known folder" from "default
-- sync target" without corrupting provider-native paths.

ALTER TABLE mailboxes ADD COLUMN delimiter TEXT;
ALTER TABLE mailboxes ADD COLUMN subscribed INTEGER NOT NULL DEFAULT 0;
ALTER TABLE mailboxes ADD COLUMN selectable INTEGER NOT NULL DEFAULT 1;
ALTER TABLE mailboxes ADD COLUMN sync_by_default INTEGER NOT NULL DEFAULT 1;
