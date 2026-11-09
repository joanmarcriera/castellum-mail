-- 0006: raw + sanitized HTML bodies for the WKWebView reading pane.
--
-- html_body keeps the ORIGINAL (unsanitized, inert at rest) so a future
-- sanitizer-policy upgrade can re-sanitize via migration without an IMAP
-- refetch, and forward/quote/"view original" have source material.
-- has_remote_content marks messages whose sanitized html still references
-- remote (http/https) sources, so the UI can offer "Load remote images".
ALTER TABLE message_bodies ADD COLUMN html_body TEXT;
ALTER TABLE message_bodies ADD COLUMN has_remote_content INTEGER NOT NULL DEFAULT 0;

-- Greenfield backfill (pre-release, all local data re-fetchable): drop sync
-- cursors so the next sync refetches every mailbox and the idempotent
-- upsert_message fills the new columns in place.
DELETE FROM sync_cursors;
