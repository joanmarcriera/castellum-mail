-- 0054 (#3446): keep an invite's RECURRENCE-ID. Without it a reply to ONE occurrence of a
-- recurring meeting (or an update/cancel of one occurrence) cannot say which instance it is
-- about, so it answered or replaced the whole series. Same flattened shape as DTSTART/DTEND
-- (raw wire value + TZID + UTC / date-only flags). Existing rows get NULL until the message is
-- next hydrated.
ALTER TABLE calendar_invites ADD COLUMN recurrence_id_raw TEXT;
ALTER TABLE calendar_invites ADD COLUMN recurrence_id_tzid TEXT;
ALTER TABLE calendar_invites ADD COLUMN recurrence_id_is_utc INTEGER NOT NULL DEFAULT 0;
ALTER TABLE calendar_invites ADD COLUMN recurrence_id_is_date_only INTEGER NOT NULL DEFAULT 0;
