-- 0053 (#3041 / #3113): keep an invite's LOCATION and DESCRIPTION. They were
-- parsed but dropped, because get_calendar_invite reads fixed columns, so the
-- invite card lost them on every reload. Both are display text, local-only
-- like the rest of the row; existing rows get NULL until the message is
-- next hydrated.
ALTER TABLE calendar_invites ADD COLUMN location TEXT;
ALTER TABLE calendar_invites ADD COLUMN description TEXT;
