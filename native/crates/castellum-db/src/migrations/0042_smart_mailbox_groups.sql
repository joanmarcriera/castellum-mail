-- 0042: smart mailboxes may hold a nested FilterGroup
-- (S40 castellum_models::FilterGroup: all/any/none groups, recursive) plus a
-- scope, instead of only an S39 text query. Both nullable JSON: a legacy
-- query mailbox keeps group_json NULL and behaves exactly as before; a group
-- mailbox stores query = '' and its conditions here. 41 is PR #173 (tags),
-- which merged first; the runner applies ids above the MAX(id) watermark.
ALTER TABLE smart_mailboxes ADD COLUMN group_json TEXT;
ALTER TABLE smart_mailboxes ADD COLUMN scope_json TEXT;
