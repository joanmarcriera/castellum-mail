-- 0027: List-Unsubscribe (RFC 2369 / RFC 8058 one-click) — S44.
--
-- Populated by Database::set_unsubscribe_targets, called once from
-- new_message_from_raw's caller (castellum-core sync.rs) right after the
-- full RFC822 body is parsed and upserted — the lightweight descriptor-only
-- sync path never has headers to read, so these stay NULL until a message
-- is hydrated (opened), matching the existing body-on-open architecture.
--
-- list_unsubscribe_mailto / list_unsubscribe_url hold the FIRST mailto:/
-- https: target found in the header (already resolved — never a raw,
-- angle-bracket-wrapped header value the UI would have to re-parse).
-- list_unsubscribe_one_click is true only when BOTH an https: target exists
-- AND List-Unsubscribe-Post literally says "List-Unsubscribe=One-Click"
-- (RFC 8058 §3.1); a bare http: (non-TLS) target is never captured as
-- list_unsubscribe_url at all, so it can never be one-click by construction.
--
-- Migration id note: the S43/S44 plan (project-docs/plans/2026-09-18-snooze-
-- and-unsubscribe.md) provisionally numbered this 0026, assuming S43 (0025,
-- snoozed_messages) would land first. S43 had not merged when this stage
-- ran, so this migration takes the next free id on this branch (0024) per
-- the roadmap's "Migration numbering" section (claimed at merge time, not
-- in the plan) — renumber on rebase if another stage claims 0024 first.
ALTER TABLE messages ADD COLUMN list_unsubscribe_mailto TEXT;
ALTER TABLE messages ADD COLUMN list_unsubscribe_url TEXT;
ALTER TABLE messages ADD COLUMN list_unsubscribe_one_click INTEGER NOT NULL DEFAULT 0;
