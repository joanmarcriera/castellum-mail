-- 0032: calendar invite response memory (S55). One row per message that
-- carries a `text/calendar` part, parsed at hydration time (same moment as
-- attachments/trust/list-unsubscribe — see castellum_core::sync::
-- new_message_from_raw). This is NOT the calendar event itself: EventKit
-- (local macOS Calendar, via `apple/`) owns that. This table is Castellum's
-- OWN memory of (a) what the invite said, kept as small display data so the
-- reading pane's invite card renders synchronously with no re-parse and no
-- network round trip, and (b) what the user already did about it, so
-- Accept/Decline/Tentative shows the right state on every later open and is
-- never re-sent. No RFC822/`.ics` source bytes are stored here or anywhere
-- else (see raw_source.rs's header comment) — never log or export
-- `attendees_json` or any other column; it is local-only, same as message
-- bodies.
CREATE TABLE calendar_invites (
    message_id          TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    uid                  TEXT NOT NULL,
    sequence             INTEGER NOT NULL DEFAULT 0,
    -- REQUEST | CANCEL | REPLY | PUBLISH | ..., upper-cased.
    method               TEXT NOT NULL,
    summary              TEXT,
    -- DTSTART/DTEND kept as the raw iCalendar wire value (see
    -- castellum_models::CalendarInviteDateTime) rather than resolved to an
    -- absolute instant — TZID resolution needs the IANA timezone database,
    -- which the Swift side already has via Foundation.
    dtstart_raw          TEXT,
    dtstart_tzid         TEXT,
    dtstart_is_utc       INTEGER NOT NULL DEFAULT 0,
    dtstart_is_date_only INTEGER NOT NULL DEFAULT 0,
    dtend_raw            TEXT,
    dtend_tzid           TEXT,
    dtend_is_utc         INTEGER NOT NULL DEFAULT 0,
    dtend_is_date_only   INTEGER NOT NULL DEFAULT 0,
    organizer_email      TEXT,
    organizer_name       TEXT,
    -- JSON array of {email, name, role, part_stat} — small (a handful of
    -- attendees), kept here so the invite card needs no extra query.
    attendees_json        TEXT NOT NULL DEFAULT '[]',
    -- NULL until the user acts; then 'accepted' | 'declined' | 'tentative'.
    our_response          TEXT,
    responded_at          INTEGER,
    updated_at            TEXT NOT NULL DEFAULT (datetime('now'))
);
