-- 0035: server drafts (S36).
--
-- A local draft can be mirrored to the account's server Drafts mailbox. NULL
-- in every column below means "never mirrored" (an ordinary local-only
-- draft, today's only kind) — the common case, so no column is NOT NULL.
-- `server_message_id` is the RFC822 Message-ID of the CURRENTLY LIVE server
-- copy: kept even when `server_uid` is temporarily unknown (APPENDUID
-- missing and the Message-ID search fallback also came up empty), so a later
-- save can still find and expunge the stale copy by searching for it again.
ALTER TABLE drafts ADD COLUMN server_mailbox_path TEXT;
ALTER TABLE drafts ADD COLUMN server_uid INTEGER;
ALTER TABLE drafts ADD COLUMN server_uid_validity INTEGER;
ALTER TABLE drafts ADD COLUMN server_message_id TEXT;
ALTER TABLE drafts ADD COLUMN server_saved_at_epoch INTEGER;
