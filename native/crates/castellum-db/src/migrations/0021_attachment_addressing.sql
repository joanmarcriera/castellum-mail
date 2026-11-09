-- 0021: incoming attachment addressing metadata (S32).
--
-- `section` is the IMAP BODYSTRUCTURE-style dot-separated part number (e.g.
-- "2", "1.2") computed from the hydrated MIME tree at parse time (see
-- castellum_mail::parse_message / section_paths). It lets the on-demand
-- attachment download path issue `UID FETCH <uid> (BODY.PEEK[<section>])`
-- without re-fetching the whole message.
--
-- `content_id`/`is_inline` mirror the MIME Content-ID/Content-Disposition
-- headers, used to forward an original message's inline images with their
-- original cid: references intact (S33).
--
-- `transfer_encoding` is the part's Content-Transfer-Encoding (lower-cased,
-- NULL = undeclared = identity). IMAP BODY[<section>] returns a part still in
-- that encoding, so the download path needs it to decode the bytes.
--
-- Existing rows (written before this migration) get section = '' — "unknown
-- section, re-hydrate the message body to learn it" — never NULL, so every
-- reader can keep using a plain TEXT NOT NULL column instead of Option<String>.
-- The download path re-hydrates such a row transparently (see
-- castellum_core::attachments).
ALTER TABLE message_attachments ADD COLUMN section TEXT NOT NULL DEFAULT '';
ALTER TABLE message_attachments ADD COLUMN content_id TEXT;
ALTER TABLE message_attachments ADD COLUMN is_inline INTEGER NOT NULL DEFAULT 0;
ALTER TABLE message_attachments ADD COLUMN transfer_encoding TEXT;
