-- 0025: Markdown compose (S46) — the sent HTML alternative for a draft.
--
-- NULL means "plain text only" (Markdown off for this message):
-- `build_rfc822` takes the exact pre-S46 single-part text/plain code path in
-- that case, byte for byte. Non-NULL is the SANITIZED, already-rendered HTML
-- `text/html` alternative for the draft's `body_text` (the Markdown SOURCE)
-- — rendered once by `castellum_mail::render_markdown_to_html` and stored
-- verbatim; it is never re-rendered at send time.
ALTER TABLE drafts ADD COLUMN body_html TEXT;
