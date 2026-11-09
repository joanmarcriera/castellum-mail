// Castellum - calendar invite detection + ICS parse/compose (S55).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Detects a `text/calendar` MIME part (inline or attached — no filename or
// Content-Disposition is required; invites commonly arrive as a plain,
// undecorated part of a multipart/mixed or multipart/alternative body)
// alongside the existing attachment walk `parse_message_checked` already
// does, and parses it with the `icalendar` crate — a small, actively
// maintained RFC 5545 parser/builder (see native/crates/castellum-mail/
// Cargo.toml's dependency comment). Dates are kept as their RAW iCalendar
// wire form (`castellum_models::CalendarInviteDateTime`) rather than
// resolved to an absolute instant: resolving a `TZID` needs the IANA
// timezone database, which the Swift side already has via Foundation, so
// Rust doesn't carry a second (and possibly divergent) copy of it.
//
// Two read entry points mirror `parse_message`/`parse_message_checked`:
//   * `parse_calendar_invite`: best-effort, `None` on anything it cannot
//     make sense of. Used by `find_calendar_invite` during ingest — a bad
//     `.ics` must never cost the rest of the message (see the
//     `catch_unwind` around the call site in `lib.rs`).
//   * `parse_calendar_invite_checked`: same parse, but returns the reason on
//     failure — what the hostile-input tests below exercise directly, and
//     available to any future caller that needs to explain a rejection
//     rather than silently show no invite card.
//
// `compose_reply_ics` builds the OUTGOING side: a minimal iTIP
// `METHOD:REPLY` calendar for Accept/Decline/Tentative, echoing the
// original UID/SEQUENCE/DTSTART/DTEND/ORGANIZER so the organizer's client
// matches the reply to the original invite. Castellum never invents a
// second mail-sending path for it — the caller (`castellum_core`) attaches
// this text to an ordinary `Draft` and sends it through the existing
// `castellum_provider_smtp::build_rfc822` outbox flow, same as any other
// reply.

use icalendar::{
    Attendee, Calendar as IcsCalendar, CalendarComponent, Component, Event as IcsEvent, PartStat,
    Property, Role,
};
use mail_parser::{Message, MimeHeaders, PartType};

use castellum_models::{CalendarInvite, CalendarInviteAttendee, CalendarInviteDateTime};

/// Why a `.ics` could not be turned into a [`CalendarInvite`]. Every variant
/// is a clean, describable refusal — never a panic. `hostile_ics_never_panics`
/// (below) fuzzes [`parse_calendar_invite_checked`] with a battery of
/// truncated/malformed/oversized/non-UTF8-ish inputs and asserts on that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalendarParseError {
    /// The text was empty or all whitespace.
    Empty,
    /// Larger than any real invite could plausibly be — refused before
    /// parsing (see `MAX_ICS_BYTES`).
    TooLarge,
    /// More `BEGIN:` component openings than any real invite has — refused
    /// before parsing, because `icalendar`'s parser recurses once per
    /// component and an unbounded/adversarial nesting depth overflows the
    /// stack (an abort, not a catchable panic — see `MAX_BEGIN_COUNT` and
    /// `hostile_ics_never_panics`, which is what caught this).
    TooDeep,
    /// The `icalendar` parser rejected it outright; carries its own reason.
    NotIcalendar(String),
    /// Parsed as a calendar, but it has no `VEVENT` component.
    NoEvent,
    /// The `VEVENT` has no (non-empty) `UID` — required by RFC 5545, and by
    /// an iTIP reply (which must echo it), so an invite without one cannot
    /// be tracked or replied to.
    MissingUid,
}

impl std::fmt::Display for CalendarParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CalendarParseError::Empty => write!(f, "empty calendar text"),
            CalendarParseError::TooLarge => write!(f, "calendar text too large"),
            CalendarParseError::TooDeep => write!(f, "calendar nesting too deep"),
            CalendarParseError::NotIcalendar(reason) => {
                write!(f, "not a valid iCalendar: {reason}")
            }
            CalendarParseError::NoEvent => write!(f, "calendar has no VEVENT"),
            CalendarParseError::MissingUid => write!(f, "VEVENT has no UID"),
        }
    }
}

impl std::error::Error for CalendarParseError {}

/// Hard cap on input size, checked BEFORE parsing. A real invite is a
/// handful of components and a few KB; this is generous headroom, not a
/// realistic ceiling.
const MAX_ICS_BYTES: usize = 256 * 1024;

/// Hard cap on `BEGIN:` occurrences, checked BEFORE parsing. `icalendar`'s
/// parser recurses once per component it descends into, so an attacker (or
/// a truncated/corrupt file) offering thousands of unclosed `BEGIN:`s can
/// overflow the stack — that's a process abort, which no `catch_unwind`
/// can turn into a clean `Err`, so it must never reach the parser at all.
/// 200 is generous: a real VEVENT nests at most a few VALARMs.
const MAX_BEGIN_COUNT: usize = 200;

/// Find the first `text/calendar` leaf part in `message` (inline or
/// attached, any disposition) and parse it. `None` when there is no such
/// part, or one exists but doesn't parse — a message with a broken invite
/// attached is still a normal message; see `CalendarParseError` for what
/// "doesn't parse" covers.
pub(crate) fn find_calendar_invite(message: &Message<'_>) -> Option<CalendarInvite> {
    for part in &message.parts {
        let Some(ct) = part.content_type() else {
            continue;
        };
        if !ct.c_type.eq_ignore_ascii_case("text") {
            continue;
        }
        let is_calendar = ct
            .c_subtype
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case("calendar"));
        if !is_calendar {
            continue;
        }
        // mail-parser decodes any text/* part (charset + transfer-encoding)
        // into `PartType::Text` already — see its `PartType` doc comment.
        // A text/calendar part that somehow parsed as something else (never
        // observed, but never assumed) is simply not readable as text here.
        let PartType::Text(text) = &part.body else {
            continue;
        };
        if let Ok(invite) = parse_calendar_invite_checked(text) {
            return Some(invite);
        }
    }
    None
}

/// Component openers as the `icalendar` parser will see them: it unfolds
/// continuation lines first and matches `BEGIN:` case-insensitively, so a
/// literal `matches("BEGIN:")` missed `begin:` and `BEG\r\n IN:` and let a
/// stack-overflowing nest through. Bounded by `MAX_ICS_BYTES` (checked first).
fn begin_count(ics: &str) -> usize {
    icalendar::parser::unfold(ics)
        .as_bytes()
        .windows(6)
        .filter(|w| w.eq_ignore_ascii_case(b"BEGIN:"))
        .count()
}

/// Best-effort parse: `None` on anything [`parse_calendar_invite_checked`]
/// would reject.
pub fn parse_calendar_invite(ics: &str) -> Option<CalendarInvite> {
    parse_calendar_invite_checked(ics).ok()
}

/// Parse raw `.ics` text into a [`CalendarInvite`]. Never panics: the
/// underlying `icalendar` parser is a `nom` combinator parser that returns
/// `Result` on malformed input (no `unwrap`/`expect`/indexing in its parse
/// path — verified by reading `icalendar::parser` at the pinned version),
/// and this function itself never indexes or unwraps anything derived from
/// `ics`.
pub fn parse_calendar_invite_checked(ics: &str) -> Result<CalendarInvite, CalendarParseError> {
    if ics.trim().is_empty() {
        return Err(CalendarParseError::Empty);
    }
    if ics.len() > MAX_ICS_BYTES {
        return Err(CalendarParseError::TooLarge);
    }
    if begin_count(ics) > MAX_BEGIN_COUNT {
        return Err(CalendarParseError::TooDeep);
    }
    let calendar: IcsCalendar = ics.parse().map_err(CalendarParseError::NotIcalendar)?;

    let method = property_ci(&calendar.properties, "METHOD")
        .unwrap_or("PUBLISH")
        .trim()
        .to_ascii_uppercase();

    let event = calendar
        .components
        .iter()
        .find_map(|c| match c {
            CalendarComponent::Event(e) => Some(e),
            _ => None,
        })
        .ok_or(CalendarParseError::NoEvent)?;

    // Read UID/SEQUENCE/SUMMARY case-insensitively too (not via the crate's
    // own `get_uid`/`get_sequence`/`get_summary`, which exact-match the
    // property key as parsed) — see `lowercase_method_and_property_names_
    // still_match` below.
    let uid = event_property_ci(event, "UID")
        .map(Property::value)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(CalendarParseError::MissingUid)?
        .to_owned();
    let sequence = event_property_ci(event, "SEQUENCE")
        .and_then(|p| p.value().trim().parse::<i64>().ok())
        .unwrap_or(0);
    let summary = event_property_ci(event, "SUMMARY")
        .map(Property::value)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let location = event_text_ci(event, "LOCATION");
    let description = event_text_ci(event, "DESCRIPTION");
    let dtstart = component_datetime(event, "DTSTART");
    // DTEND wins; with only DURATION the end is DTSTART + DURATION (RFC 5545
    // §3.6.1), so the card, Add to Calendar and the reply all see a real end.
    let dtend = component_datetime(event, "DTEND").or_else(|| {
        let start = dtstart.as_ref()?;
        let secs = parse_ical_duration(event_property_ci(event, "DURATION")?.value())?;
        add_duration(start, secs)
    });
    let recurrence_id = component_datetime(event, "RECURRENCE-ID");
    let (organizer_email, organizer_name) = organizer(event);

    let attendees = event
        .get_attendees()
        .into_iter()
        .map(|a| CalendarInviteAttendee {
            email: strip_mailto(&a.cal_address),
            name: a.cn,
            role: a.role.map(role_str).map(str::to_owned),
            part_stat: a.part_stat.map(part_stat_str).map(str::to_owned),
        })
        .collect();

    Ok(CalendarInvite {
        uid,
        sequence,
        method,
        summary,
        dtstart,
        dtend,
        organizer_email,
        organizer_name,
        attendees,
        location,
        description,
        our_response: None,
        responded_at: None,
        recurrence_id,
        superseded: None,
        possibly_superseded: None,
    })
}

/// Longest LOCATION / DESCRIPTION kept (display text; a hostile or huge
/// description must not bloat the row).
const MAX_INVITE_TEXT_CHARS: usize = 4000;

/// A text property, case-insensitive name, trimmed, RFC 5545 §3.3.11 escapes
/// (`\n`, `\,`, `\;`, `\\`) undone, capped. `None` when absent or blank.
fn event_text_ci(event: &icalendar::Event, name: &str) -> Option<String> {
    let raw = event_property_ci(event, name)?.value();
    let mut out = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    let text: String = out.trim().chars().take(MAX_INVITE_TEXT_CHARS).collect();
    (!text.is_empty()).then_some(text)
}

/// Build a minimal iTIP `METHOD:REPLY` `.ics` for an Accept/Decline/
/// Tentative response. `response` is `"accepted"`, `"declined"` or
/// `"tentative"`; any other value degrades to `"accepted"` — callers
/// validate the three-way choice before reaching here (see
/// `castellum_core`'s invite-reply entry point).
pub fn compose_reply_ics(
    invite: &CalendarInvite,
    response: &str,
    our_email: &str,
    our_name: Option<&str>,
) -> Result<String, CalendarParseError> {
    let part_stat = match response {
        "declined" => PartStat::Declined,
        "tentative" => PartStat::Tentative,
        _ => PartStat::Accepted,
    };

    let mut event = IcsEvent::new();
    event.uid(&invite.uid);
    event.sequence(u32::try_from(invite.sequence.max(0)).unwrap_or(0));
    if let Some(summary) = &invite.summary {
        event.summary(summary);
    }
    // DTSTART/DTEND are optional in an iTIP REPLY (UID, SEQUENCE, ORGANIZER,
    // ATTENDEE and DTSTAMP identify the invite). A TZID-qualified time would
    // need a matching VTIMEZONE block, which we don't carry, so it is left out
    // rather than emitted dangling (audit #3446).
    // RECURRENCE-ID is echoed whatever its form: without it the reply answers the whole series
    // instead of the one occurrence (#3446). A TZID on it is kept (it identifies the instance).
    if let Some(rid) = invite.recurrence_id.as_ref() {
        event.append_property(raw_datetime_property("RECURRENCE-ID", rid));
    }
    if let Some(dt) = invite.dtstart.as_ref().filter(|d| d.tzid.is_none()) {
        event.append_property(raw_datetime_property("DTSTART", dt));
    }
    if let Some(dt) = invite.dtend.as_ref().filter(|d| d.tzid.is_none()) {
        event.append_property(raw_datetime_property("DTEND", dt));
    }
    if let Some(organizer_email) = &invite.organizer_email {
        let mut organizer_prop = Property::new("ORGANIZER", format!("mailto:{organizer_email}"));
        if let Some(name) = &invite.organizer_name {
            organizer_prop.append_parameter(("CN", name.as_str()));
        }
        event.append_property(organizer_prop.done());
    }
    let mut attendee = Attendee::new(format!("mailto:{our_email}")).partstat(part_stat);
    if let Some(name) = our_name {
        attendee = attendee.cn(name.to_owned());
    }
    event.attendee(attendee);

    let mut calendar = IcsCalendar::empty();
    calendar
        .append_property(Property::new("VERSION", "2.0"))
        .append_property(Property::new("PRODID", "-//Castellum//Castellum Mail//EN"))
        .append_property(Property::new("METHOD", "REPLY"))
        .push(event.done());

    (&calendar).try_into().map_err(|_: std::fmt::Error| {
        CalendarParseError::NotIcalendar("failed to render the reply calendar".into())
    })
}

// --- helpers -----------------------------------------------------------

/// RFC 5545 DURATION (`[+-]P[nW][nD][T[nH][nM][nS]]`) to whole seconds. Only a
/// positive duration is useful for an end time; zero, negative or malformed
/// input gives `None`.
fn parse_ical_duration(raw: &str) -> Option<i64> {
    let s = raw.trim();
    let s = s.strip_prefix('+').unwrap_or(s);
    let s = s.strip_prefix(['P', 'p'])?;
    let (date_part, time_part) = match s.split_once(['T', 't']) {
        Some((d, t)) => (d, t),
        None => (s, ""),
    };
    let mut total: i64 = 0;
    let mut take = |part: &str, units: &[(char, i64)]| -> Option<()> {
        let mut num = String::new();
        for c in part.chars() {
            if c.is_ascii_digit() {
                num.push(c);
                continue;
            }
            let mult = units.iter().find(|(u, _)| u.eq_ignore_ascii_case(&c))?.1;
            let n: i64 = num.parse().ok()?;
            total = total.checked_add(n.checked_mul(mult)?)?;
            num.clear();
        }
        num.is_empty().then_some(())
    };
    take(date_part, &[('W', 604_800), ('D', 86_400)])?;
    take(time_part, &[('H', 3600), ('M', 60), ('S', 1)])?;
    // Untrusted input: refuse anything beyond ~100 years so later date maths
    // can never overflow.
    (total > 0 && total <= MAX_DURATION_SECS).then_some(total)
}

/// About 100 years: the largest DURATION an invite end time will be built from.
const MAX_DURATION_SECS: i64 = 100 * 366 * 86_400;

/// Days since 1970-01-01 for a proleptic Gregorian civil date (Hinnant).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `start` plus `secs`, in the same form (UTC `Z`, floating/TZID local, or
/// all-day). An all-day start only accepts whole days (the end then stays an
/// exclusive date, like DTEND); anything unparseable gives `None`.
fn add_duration(start: &CalendarInviteDateTime, secs: i64) -> Option<CalendarInviteDateTime> {
    if !(0..=MAX_DURATION_SECS).contains(&secs) {
        return None;
    }
    let raw = start.raw.trim_end_matches('Z');
    let digits = |r: std::ops::Range<usize>| raw.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (digits(0..4)?, digits(4..6)?, digits(6..8)?);
    if !(1..=9999).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let (hh, mm, ss) = if start.is_date_only {
        if secs % 86_400 != 0 {
            return None;
        }
        (0, 0, 0)
    } else {
        if raw.get(8..9) != Some("T") {
            return None;
        }
        (digits(9..11)?, digits(11..13)?, digits(13..15)?)
    };
    if !(0..=23).contains(&hh) || !(0..=59).contains(&mm) || !(0..=60).contains(&ss) {
        return None;
    }
    let t = days_from_civil(y, m, d)
        .checked_mul(86_400)?
        .checked_add(hh * 3600 + mm * 60 + ss)?
        .checked_add(secs)?;
    let (days, rem) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    if !(1..=9999).contains(&y) {
        return None;
    }
    let new_raw = if start.is_date_only {
        format!("{y:04}{m:02}{d:02}")
    } else {
        let z = if start.is_utc { "Z" } else { "" };
        format!(
            "{y:04}{m:02}{d:02}T{:02}{:02}{:02}{z}",
            rem / 3600,
            rem % 3600 / 60,
            rem % 60
        )
    };
    Some(CalendarInviteDateTime {
        raw: new_raw,
        tzid: start.tzid.clone(),
        is_utc: start.is_utc,
        is_date_only: start.is_date_only,
    })
}

/// Case-insensitive lookup over a `Vec<Property>` (top-level `VCALENDAR`
/// properties, e.g. `METHOD`) — the `icalendar` crate itself only offers an
/// exact-match `Calendar::property_value`, and real producers occasionally
/// lower-case standard names.
fn property_ci<'a>(props: &'a [Property], key: &str) -> Option<&'a str> {
    props
        .iter()
        .find(|p| p.key().eq_ignore_ascii_case(key))
        .map(Property::value)
}

/// Case-insensitive lookup over an event's own properties (`DTSTART`,
/// `ORGANIZER`, ...). Same rationale as `property_ci`.
fn event_property_ci<'a>(event: &'a icalendar::Event, key: &str) -> Option<&'a Property> {
    event
        .properties()
        .values()
        .find(|p| p.key().eq_ignore_ascii_case(key))
}

/// A property parameter's value, looked up case-insensitively (`TZID`,
/// `VALUE`, `CN`, ...).
fn param_ci<'a>(prop: &'a Property, key: &str) -> Option<&'a str> {
    prop.params()
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.value())
}

fn component_datetime(event: &icalendar::Event, key: &str) -> Option<CalendarInviteDateTime> {
    let prop = event_property_ci(event, key)?;
    let raw = prop.value().trim().to_owned();
    if raw.is_empty() {
        return None;
    }
    let tzid = param_ci(prop, "TZID")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let is_date_only = param_ci(prop, "VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE"));
    // FORM #2 (UTC) only applies when there's no TZID and it's not a bare
    // date — a `Z`-suffixed *date* isn't valid iCalendar, but defensively
    // this only ever flags true for an actual date-time value.
    let is_utc = tzid.is_none() && !is_date_only && raw.ends_with('Z');
    Some(CalendarInviteDateTime {
        raw,
        tzid,
        is_utc,
        is_date_only,
    })
}

/// The ORGANIZER is where an Accept/Decline reply is SENT, and a hostile
/// sender writes it. It is kept only when it is exactly one plain addr-spec
/// ([`validate_plain_addr_spec`]); anything else (bidi or homoglyph address,
/// several addresses, mailto parameters) is dropped, which leaves the invite
/// readable but not replyable (#3472). The CN is display text next to that
/// address and is reduced by [`sanitise_display_name`]; with no valid address
/// there is no name either.
fn organizer(event: &icalendar::Event) -> (Option<String>, Option<String>) {
    let Some(prop) = event_property_ci(event, "ORGANIZER") else {
        return (None, None);
    };
    let Some(email) = validate_plain_addr_spec(&strip_mailto(prop.value().trim())) else {
        return (None, None);
    };
    let name = param_ci(prop, "CN").and_then(sanitise_display_name);
    (Some(email), name)
}

/// Longest display name kept for an organizer. A longer CN is how a hostile
/// invite pushes the real address out of view in "Name <address>".
pub const MAX_DISPLAY_NAME_CHARS: usize = 64;

/// `Some(address)` when `candidate` is exactly ONE plain addr-spec that is
/// safe to show and to send to, else `None`. Deliberately narrower than
/// RFC 5322/6531 (fail closed, #3472):
///   * printable ASCII only: no bidi controls, no zero-width or other
///     invisible characters, no homoglyph letters (an internationalised
///     domain must arrive as punycode, which is visibly `xn--`), no spaces;
///   * exactly one `@`; a dot-atom local part of 1..=64 characters (no quoted
///     string, no leading, trailing or doubled dot);
///   * a DNS host name of at least two labels (letters, digits, inner
///     hyphens; no address literal, no all-numeric last label), 253 at most;
///   * nothing else: no display name, angle brackets, comma-separated list,
///     or `?cc=`/`?bcc=` mailto parameters.
///
/// The address is returned unchanged (case preserved).
pub fn validate_plain_addr_spec(candidate: &str) -> Option<String> {
    if candidate.is_empty() || candidate.len() > 254 {
        return None;
    }
    if !candidate.bytes().all(|b| b.is_ascii_graphic()) {
        return None;
    }
    let (local, domain) = candidate.split_once('@')?;
    if domain.contains('@') {
        return None;
    }
    let local_char_ok = |b: u8| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'.' | b'!'
                    | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'/'
                    | b'='
                    | b'?'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'{'
                    | b'|'
                    | b'}'
                    | b'~'
            )
    };
    if local.is_empty()
        || local.len() > 64
        || !local.bytes().all(local_char_ok)
        || local.starts_with('.')
        || local.ends_with('.')
        || local.contains("..")
    {
        return None;
    }
    if domain.len() > 253 {
        return None;
    }
    let labels: Vec<&str> = domain.split('.').collect();
    let label_ok = |l: &&str| {
        !l.is_empty()
            && l.len() <= 63
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    if labels.len() < 2 || !labels.iter().all(label_ok) {
        return None;
    }
    if labels
        .last()
        .is_some_and(|tld| tld.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    Some(candidate.to_owned())
}

/// Reduce attacker-written display text (an ORGANIZER `CN`) to something safe
/// to show beside an address: control, bidi and other invisible format
/// characters are dropped, quotes and angle brackets too (they could fake a
/// second `<address>`), whitespace runs collapse to one space, and the result
/// is capped at [`MAX_DISPLAY_NAME_CHARS`]. `None` when nothing is left, or
/// when the name itself contains an `@` in any form: a name that reads like
/// an address exists to be mistaken for the real one (#3472).
pub fn sanitise_display_name(name: &str) -> Option<String> {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| {
            !c.is_control()
                && !matches!(c, '"' | '<' | '>')
                && !matches!(
                    *c as u32,
                    0x00AD
                        | 0x061C
                        | 0x115F
                        | 0x1160
                        | 0x180E
                        | 0x200B..=0x200F
                        | 0x202A..=0x202E
                        | 0x2060..=0x206F
                        | 0x3164
                        | 0xFE00..=0xFE0F
                        | 0xFEFF
                        | 0xFFA0
                        | 0xFFF9..=0xFFFB
                        | 0xE0000..=0xE0FFF
                )
        })
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() || cleaned.contains(['@', '\u{FF20}', '\u{FE6B}']) {
        return None;
    }
    let capped: String = cleaned.chars().take(MAX_DISPLAY_NAME_CHARS).collect();
    Some(capped.trim_end().to_owned())
}

fn raw_datetime_property(key: &str, dt: &CalendarInviteDateTime) -> Property {
    let mut prop = Property::new(key, dt.raw.as_str());
    if let Some(tzid) = &dt.tzid {
        prop.append_parameter(("TZID", tzid.as_str()));
    }
    if dt.is_date_only {
        prop.append_parameter(("VALUE", "DATE"));
    }
    prop.done()
}

/// Strip a leading `mailto:`/`MAILTO:` scheme and surrounding quotes. Never
/// panics on anything, including a value that isn't a `mailto:` URI at all
/// (returned trimmed, unchanged) — some producers put a bare address in
/// `ORGANIZER`/`ATTENDEE`.
fn strip_mailto(addr: &str) -> String {
    let addr = addr.trim();
    let addr = match addr.get(..7) {
        Some(prefix) if prefix.eq_ignore_ascii_case("mailto:") => &addr[7..],
        _ => addr,
    };
    addr.trim_matches('"').to_owned()
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::Chair => "CHAIR",
        Role::ReqParticipant => "REQ-PARTICIPANT",
        Role::OptParticipant => "OPT-PARTICIPANT",
        Role::NonParticipant => "NON-PARTICIPANT",
    }
}

fn part_stat_str(p: PartStat) -> &'static str {
    match p {
        PartStat::NeedsAction => "NEEDS-ACTION",
        PartStat::Accepted => "ACCEPTED",
        PartStat::Declined => "DECLINED",
        PartStat::Tentative => "TENTATIVE",
        PartStat::Delegated => "DELEGATED",
        PartStat::Completed => "COMPLETED",
        PartStat::InProcess => "IN-PROCESS",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WELL_FORMED: &str = "BEGIN:VCALENDAR\r\n\
        VERSION:2.0\r\n\
        PRODID:-//Test//EN\r\n\
        METHOD:REQUEST\r\n\
        BEGIN:VEVENT\r\n\
        UID:event-123@example.com\r\n\
        SEQUENCE:2\r\n\
        DTSTAMP:20260101T090000Z\r\n\
        DTSTART:20260925T140000Z\r\n\
        DTEND:20260925T150000Z\r\n\
        SUMMARY:Roadmap review\r\n\
        ORGANIZER;CN=Alice Organizer:mailto:alice@example.com\r\n\
        ATTENDEE;CN=Bob Attendee;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION:mailto:bob@example.com\r\n\
        END:VEVENT\r\n\
        END:VCALENDAR\r\n";

    #[test]
    fn parses_method_uid_sequence_times_organizer_and_attendees() {
        let invite = parse_calendar_invite_checked(WELL_FORMED).expect("well-formed ICS parses");
        assert_eq!(invite.method, "REQUEST");
        assert_eq!(invite.uid, "event-123@example.com");
        assert_eq!(invite.sequence, 2);
        assert_eq!(invite.summary.as_deref(), Some("Roadmap review"));
        let dtstart = invite.dtstart.expect("DTSTART present");
        assert_eq!(dtstart.raw, "20260925T140000Z");
        assert!(dtstart.is_utc);
        assert!(!dtstart.is_date_only);
        assert_eq!(invite.dtend.expect("DTEND present").raw, "20260925T150000Z");
        assert_eq!(invite.organizer_email.as_deref(), Some("alice@example.com"));
        assert_eq!(invite.organizer_name.as_deref(), Some("Alice Organizer"));
        assert_eq!(invite.attendees.len(), 1);
        let attendee = &invite.attendees[0];
        assert_eq!(attendee.email, "bob@example.com");
        assert_eq!(attendee.name.as_deref(), Some("Bob Attendee"));
        assert_eq!(attendee.role.as_deref(), Some("REQ-PARTICIPANT"));
        assert_eq!(attendee.part_stat.as_deref(), Some("NEEDS-ACTION"));
    }

    #[test]
    fn tzid_qualified_datetime_is_kept_raw_not_guessed_as_utc() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\n\
            BEGIN:VEVENT\r\nUID:x@example.com\r\n\
            DTSTART;TZID=Europe/London:20260925T140000\r\n\
            END:VEVENT\r\nEND:VCALENDAR\r\n";
        let invite = parse_calendar_invite_checked(ics).unwrap();
        let dtstart = invite.dtstart.unwrap();
        assert_eq!(dtstart.tzid.as_deref(), Some("Europe/London"));
        assert!(!dtstart.is_utc);
        assert_eq!(dtstart.raw, "20260925T140000");
    }

    #[test]
    fn all_day_value_date_is_flagged_date_only() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\n\
            BEGIN:VEVENT\r\nUID:x@example.com\r\n\
            DTSTART;VALUE=DATE:20260925\r\n\
            END:VEVENT\r\nEND:VCALENDAR\r\n";
        let invite = parse_calendar_invite_checked(ics).unwrap();
        let dtstart = invite.dtstart.unwrap();
        assert!(dtstart.is_date_only);
        assert!(!dtstart.is_utc);
        assert_eq!(dtstart.raw, "20260925");
    }

    #[test]
    fn missing_sequence_defaults_to_zero() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\n\
            BEGIN:VEVENT\r\nUID:x@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(parse_calendar_invite_checked(ics).unwrap().sequence, 0);
    }

    #[test]
    fn missing_method_defaults_to_publish() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
            BEGIN:VEVENT\r\nUID:x@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(
            parse_calendar_invite_checked(ics).unwrap().method,
            "PUBLISH"
        );
    }

    #[test]
    fn lowercase_method_and_property_names_still_match() {
        // RFC 5545 contentline names are case-insensitive; some producers
        // (and every hostile fuzzer) don't use the conventional upper case.
        let ics = "begin:VCALENDAR\r\nversion:2.0\r\nmethod:cancel\r\n\
            BEGIN:VEVENT\r\nuid:x@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let invite = parse_calendar_invite_checked(ics).unwrap();
        assert_eq!(invite.method, "CANCEL");
        assert_eq!(invite.uid, "x@example.com");
    }

    // --- hostile / malformed input: must error cleanly, never panic -------

    #[test]
    fn hostile_ics_never_panics() {
        let hostile: &[&str] = &[
            "",
            "   \n\t  ",
            "not even remotely a calendar",
            "BEGIN:VCALENDAR",                     // truncated, no END
            "BEGIN:VEVENT\r\nEND:VEVENT\r\n",       // VEVENT with no VCALENDAR wrapper
            "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n", // no VEVENT at all
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n", // VEVENT, no UID
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n", // empty UID
            "BEGIN:VCALENDAR\r\nMETHOD\r\nEND:VCALENDAR\r\n", // property with no colon
            "BEGIN:VCALENDAR\r\n:::::\r\nEND:VCALENDAR\r\n",
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x\r\nSEQUENCE:not-a-number\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x\r\nDTSTART:not-a-date\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x\r\nATTENDEE;PARTSTAT=BOGUS:not-an-email\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
            // Deeply nested BEGIN with no matching END — a naive recursive
            // descent parser is exactly what this would blow a stack on.
            &"BEGIN:VALARM\r\n".repeat(5_000),
            // A single absurdly long unfolded line.
            &format!("BEGIN:VCALENDAR\r\nX-HUGE:{}\r\nEND:VCALENDAR\r\n", "a".repeat(200_000)),
            // Raw bytes that are invalid as a contentline but valid UTF-8.
            "BEGIN:VCALENDAR\r\n\u{0}\u{1}\u{7f}\r\nEND:VCALENDAR\r\n",
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x\r\nDTSTART;TZID=:20260101T000000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        ];
        for input in hostile {
            // `catch_unwind` makes the "never panics" claim load-bearing
            // rather than merely aspirational: if `icalendar` (or this
            // module) ever regresses into a panic on adversarial input,
            // this test fails loudly instead of silently trusting the
            // Result type. `AssertUnwindSafe` is fine — this is a pure
            // function borrowing only `&str` and touching no shared state.
            // The load-bearing assertion: whatever `icalendar` or this
            // module does with adversarial input, it must return, never
            // unwind. A leniently-recovered `Ok` (e.g. a garbage SEQUENCE
            // silently defaulting to 0, same as a compliant reader would)
            // is an acceptable outcome for some entries in this corpus —
            // the DEDICATED tests above (`empty_and_whitespace_only_...`,
            // `calendar_with_no_vevent_...`, `vevent_with_no_uid_...`) pin
            // the exact `CalendarParseError` for the shapes that must
            // always be a clean, typed refusal.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                parse_calendar_invite_checked(input)
            }));
            assert!(
                result.is_ok(),
                "parse_calendar_invite_checked panicked on {input:?}"
            );
        }
    }

    #[test]
    fn empty_and_whitespace_only_input_is_a_clean_empty_error() {
        assert_eq!(
            parse_calendar_invite_checked(""),
            Err(CalendarParseError::Empty)
        );
        assert_eq!(
            parse_calendar_invite_checked("   \n\t "),
            Err(CalendarParseError::Empty)
        );
    }

    #[test]
    fn calendar_with_no_vevent_is_a_clean_no_event_error() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nEND:VCALENDAR\r\n";
        assert_eq!(
            parse_calendar_invite_checked(ics),
            Err(CalendarParseError::NoEvent)
        );
    }

    #[test]
    fn deeply_nested_begin_is_rejected_before_it_can_overflow_the_stack() {
        // Regression: `icalendar`'s recursive-descent parser recurses once
        // per `BEGIN:`, and 5,000 unclosed ones overflowed the stack (an
        // abort — SIGABRT, not a `panic!` — which `catch_unwind` cannot
        // intercept) before `MAX_BEGIN_COUNT` was added.
        let hostile = "BEGIN:VALARM\r\n".repeat(5_000);
        assert_eq!(
            parse_calendar_invite_checked(&hostile),
            Err(CalendarParseError::TooDeep)
        );
    }

    #[test]
    fn nesting_cap_counts_what_the_parser_sees_lowercase_and_folded() {
        // Regression: the cap counted only the literal `BEGIN:`, but the
        // parser unfolds continuation lines first and matches `begin:`
        // case-insensitively, so both forms below reached the recursive
        // parser uncounted and overflowed the stack (an abort).
        for hostile in [
            "begin:VALARM\r\n".repeat(5_000),
            "BEG\r\n IN:VALARM\r\n".repeat(5_000),
        ] {
            assert_eq!(
                parse_calendar_invite_checked(&hostile),
                Err(CalendarParseError::TooDeep)
            );
        }
    }

    #[test]
    fn oversized_ics_is_rejected_before_parsing() {
        let hostile = format!(
            "BEGIN:VCALENDAR\r\nX-HUGE:{}\r\nEND:VCALENDAR\r\n",
            "a".repeat(300_000)
        );
        assert_eq!(
            parse_calendar_invite_checked(&hostile),
            Err(CalendarParseError::TooLarge)
        );
    }

    #[test]
    fn vevent_with_no_uid_is_a_clean_missing_uid_error() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
            BEGIN:VEVENT\r\nSUMMARY:no uid here\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(
            parse_calendar_invite_checked(ics),
            Err(CalendarParseError::MissingUid)
        );
    }

    #[test]
    fn find_calendar_invite_detects_an_inline_part_with_no_disposition() {
        // Real invites commonly arrive as a plain multipart/alternative leaf
        // with no filename and no Content-Disposition at all.
        let raw = format!(
            "From: organizer@example.com\r\n\
             To: me@example.com\r\n\
             Subject: Meeting\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: multipart/alternative; boundary=BOUNDARY\r\n\
             \r\n\
             --BOUNDARY\r\n\
             Content-Type: text/plain\r\n\
             \r\n\
             See invite.\r\n\
             --BOUNDARY\r\n\
             Content-Type: text/calendar; method=REQUEST; charset=UTF-8\r\n\
             \r\n\
             {WELL_FORMED}\
             --BOUNDARY--\r\n"
        );
        let message = mail_parser::MessageParser::default()
            .parse(raw.as_bytes())
            .expect("parses as a message");
        let invite = find_calendar_invite(&message).expect("finds the inline text/calendar part");
        assert_eq!(invite.uid, "event-123@example.com");
        assert_eq!(invite.method, "REQUEST");
    }

    #[test]
    fn find_calendar_invite_returns_none_for_a_message_with_no_calendar_part() {
        let raw = b"From: a@b\r\nTo: c@d\r\nSubject: t\r\n\r\njust text";
        let message = mail_parser::MessageParser::default()
            .parse(raw.as_ref())
            .expect("parses as a message");
        assert!(find_calendar_invite(&message).is_none());
    }

    #[test]
    fn find_calendar_invite_never_panics_on_a_hostile_calendar_part() {
        let raw = "From: a@b\r\nTo: c@d\r\nSubject: t\r\n\
            MIME-Version: 1.0\r\n\
            Content-Type: multipart/mixed; boundary=B\r\n\r\n\
            --B\r\nContent-Type: text/calendar\r\n\r\nBEGIN:VCALENDAR\r\n--B--\r\n";
        let message = mail_parser::MessageParser::default()
            .parse(raw.as_bytes())
            .expect("parses as a message");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            find_calendar_invite(&message)
        }));
        assert!(result.is_ok(), "find_calendar_invite panicked");
        assert_eq!(
            result.unwrap(),
            None,
            "truncated calendar part yields no invite"
        );
    }

    // --- compose_reply_ics --------------------------------------------------

    fn sample_invite() -> CalendarInvite {
        parse_calendar_invite_checked(WELL_FORMED).unwrap()
    }

    #[test]
    fn reply_echoes_uid_sequence_and_carries_our_partstat() {
        let invite = sample_invite();
        let ics = compose_reply_ics(&invite, "declined", "me@example.com", Some("Me"))
            .expect("reply composes");
        assert!(ics.contains("METHOD:REPLY"));
        assert!(ics.contains("UID:event-123@example.com"));
        assert!(ics.contains("SEQUENCE:2"));
        assert!(ics.contains("PARTSTAT=DECLINED"));
        assert!(ics.contains("mailto:me@example.com"));
        assert!(ics.contains("ORGANIZER"));
        assert!(ics.contains("alice@example.com"));
        // The reply itself must re-parse cleanly (round-trip sanity).
        let reparsed = parse_calendar_invite_checked(&ics).expect("reply is valid ICS");
        assert_eq!(reparsed.method, "REPLY");
        assert_eq!(reparsed.uid, invite.uid);
    }

    /// #3446: a reply to ONE occurrence of a recurring meeting must say which occurrence, or it
    /// answers the whole series.
    #[test]
    fn recurrence_id_is_parsed_and_echoed_in_the_reply() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\n\
                   BEGIN:VEVENT\r\nUID:series@example.com\r\nSEQUENCE:1\r\n\
                   RECURRENCE-ID:20260930T090000Z\r\nDTSTART:20260930T100000Z\r\n\
                   ORGANIZER:mailto:alice@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let invite = parse_calendar_invite_checked(ics).unwrap();
        let rid = invite.recurrence_id.as_ref().expect("RECURRENCE-ID parsed");
        assert_eq!(rid.raw, "20260930T090000Z");
        assert!(rid.is_utc);
        let reply = compose_reply_ics(&invite, "accepted", "me@example.com", None).unwrap();
        assert!(reply.contains("RECURRENCE-ID:20260930T090000Z"), "{reply}");
        // A series-level invite carries none and the reply has none either.
        let plain =
            compose_reply_ics(&sample_invite(), "accepted", "me@example.com", None).unwrap();
        assert!(!plain.contains("RECURRENCE-ID"));
    }

    fn invite_with_organizer(organizer_line: &str) -> CalendarInvite {
        let ics = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\n\
             BEGIN:VEVENT\r\nUID:x@example.com\r\nDTSTART:20260925T140000Z\r\n\
             {organizer_line}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        );
        parse_calendar_invite_checked(&ics).expect("the invite itself still parses")
    }

    /// #3472 item 4: the ORGANIZER is the reply's recipient and is written by
    /// the sender. Anything but one plain addr-spec is not stored at all.
    #[test]
    fn hostile_organizer_addresses_are_not_stored() {
        for organizer in [
            // Right-to-left override: renders as "alice@example.com".
            "ORGANIZER:mailto:\u{202E}moc.elpmaxe@ecila\u{202C}@evil.example",
            "ORGANIZER:mailto:alice@example.com\u{202E}moc.live@",
            // Zero-width and invisible characters.
            "ORGANIZER:mailto:alice\u{200B}@example.com",
            "ORGANIZER:mailto:alice@exam\u{2060}ple.com",
            // Homoglyphs: Cyrillic a in the local part, Cyrillic e in the domain.
            "ORGANIZER:mailto:\u{430}lice@example.com",
            "ORGANIZER:mailto:alice@\u{435}xample.com",
            // Full-width separator, more than one address, mailto parameters.
            "ORGANIZER:mailto:alice\u{FF20}example.com",
            "ORGANIZER:mailto:alice@example.com,eve@evil.example",
            "ORGANIZER:mailto:alice@example.com?cc=eve@evil.example",
            "ORGANIZER:mailto:alice@example.com?subject=x",
            "ORGANIZER:mailto:alice@example.com@evil.example",
            "ORGANIZER:mailto:Alice <alice@example.com>",
            "ORGANIZER:mailto:alice@example.com eve@evil.example",
            // Not an address.
            "ORGANIZER:mailto:alice",
            "ORGANIZER:mailto:@example.com",
            "ORGANIZER:mailto:alice@localhost",
            "ORGANIZER:mailto:alice@[192.168.0.1]",
            "ORGANIZER:mailto:alice@192.168.0.1",
            "ORGANIZER:mailto:.alice@example.com",
            "ORGANIZER:mailto:al..ice@example.com",
            "ORGANIZER:mailto:alice@-example.com",
            "ORGANIZER:mailto:alice@example..com",
            "ORGANIZER:https://evil.example/alice@example.com",
        ] {
            let invite = invite_with_organizer(organizer);
            assert_eq!(invite.organizer_email, None, "{organizer:?}");
            assert_eq!(invite.organizer_name, None, "{organizer:?}");
        }
        let long_local = format!("ORGANIZER:mailto:{}@example.com", "a".repeat(65));
        assert_eq!(invite_with_organizer(&long_local).organizer_email, None);
    }

    #[test]
    fn plain_organizer_addresses_are_kept_unchanged() {
        for (line, expected) in [
            ("ORGANIZER:mailto:alice@example.com", "alice@example.com"),
            (
                "ORGANIZER:MAILTO:Alice.B+tag@Example.CO.UK",
                "Alice.B+tag@Example.CO.UK",
            ),
            ("ORGANIZER:o'brien@example.com", "o'brien@example.com"),
            (
                "ORGANIZER:mailto:alice@xn--bcher-kva.example",
                "alice@xn--bcher-kva.example",
            ),
        ] {
            assert_eq!(
                invite_with_organizer(line).organizer_email.as_deref(),
                Some(expected),
                "{line}"
            );
        }
    }

    /// #3472 item 4: a long or dressed-up CN must not push the real address
    /// out of view or impersonate one.
    #[test]
    fn organizer_cn_is_sanitised_capped_or_dropped() {
        // Long CN pushing "<attacker@evil.example>" off the visible line.
        let long = format!(
            "ORGANIZER;CN=\"Your Manager{}\":mailto:attacker@evil.example",
            " ".repeat(20) + &"x".repeat(400)
        );
        let invite = invite_with_organizer(&long);
        assert_eq!(
            invite.organizer_email.as_deref(),
            Some("attacker@evil.example")
        );
        let name = invite.organizer_name.expect("a capped name");
        assert!(name.chars().count() <= MAX_DISPLAY_NAME_CHARS, "{name}");
        assert!(name.starts_with("Your Manager x"), "{name}");

        // Bidi and zero-width characters are dropped, not kept.
        let invite = invite_with_organizer(
            "ORGANIZER;CN=\"Al\u{202E}ice\u{200B} \u{2066}Boss\u{2069}\":mailto:a@evil.example",
        );
        assert_eq!(invite.organizer_name.as_deref(), Some("Alice Boss"));

        // A name that reads like an address is dropped entirely.
        for cn in [
            "ceo@bank.example",
            "ceo\u{FF20}bank.example",
            "CEO <ceo@bank.example>",
        ] {
            let invite =
                invite_with_organizer(&format!("ORGANIZER;CN=\"{cn}\":mailto:a@evil.example"));
            assert_eq!(invite.organizer_email.as_deref(), Some("a@evil.example"));
            assert_eq!(invite.organizer_name, None, "{cn}");
        }
        assert_eq!(sanitise_display_name("\u{200B}\u{202E} \t"), None);
        assert_eq!(
            sanitise_display_name("A \"B\" <C>\r\nD").as_deref(),
            Some("A B C D")
        );
    }

    #[test]
    fn reply_accepts_and_tentative_map_to_the_right_partstat() {
        let invite = sample_invite();
        for (response, expected) in [
            ("accepted", "PARTSTAT=ACCEPTED"),
            ("tentative", "PARTSTAT=TENTATIVE"),
            ("declined", "PARTSTAT=DECLINED"),
            // Anything else degrades to Accepted rather than composing
            // something ambiguous.
            ("bogus", "PARTSTAT=ACCEPTED"),
        ] {
            let ics = compose_reply_ics(&invite, response, "me@example.com", None).unwrap();
            assert!(
                ics.contains(expected),
                "{response} -> expected {expected} in {ics}"
            );
        }
    }

    /// #3041: LOCATION and DESCRIPTION survive parsing, escapes undone.
    #[test]
    fn location_and_description_are_kept_and_unescaped() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\n\
            UID:x@example.com\r\nDTSTART:20260925T140000Z\r\nSUMMARY:Review\r\n\
            LOCATION:Room 4\\, Building B\r\nDESCRIPTION:Agenda:\\n1. Roadmap\\; costs\r\n\
            END:VEVENT\r\nEND:VCALENDAR\r\n";
        let invite = parse_calendar_invite_checked(ics).unwrap();
        assert_eq!(invite.location.as_deref(), Some("Room 4, Building B"));
        assert_eq!(
            invite.description.as_deref(),
            Some("Agenda:\n1. Roadmap; costs")
        );
        let none = parse_calendar_invite_checked(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:y\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        )
        .unwrap();
        assert_eq!((none.location, none.description), (None, None));
    }

    #[test]
    fn duration_without_dtend_synthesises_the_end() {
        let ics = |start: &str, dur: &str| {
            format!(
                "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:d@x\r\n{start}\r\n{dur}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
            )
        };
        let end = |s: &str, d: &str| parse_calendar_invite(&ics(s, d)).unwrap().dtend;
        let utc = end("DTSTART:20260925T233000Z", "DURATION:PT45M").unwrap();
        assert_eq!((utc.raw.as_str(), utc.is_utc), ("20260926T001500Z", true));
        let tz = end(
            "DTSTART;TZID=Europe/London:20260925T140000",
            "DURATION:PT2H",
        )
        .unwrap();
        assert_eq!(
            (tz.raw.as_str(), tz.tzid.as_deref()),
            ("20260925T160000", Some("Europe/London"))
        );
        let day = end("DTSTART;VALUE=DATE:20261231", "DURATION:P2D").unwrap();
        assert_eq!((day.raw.as_str(), day.is_date_only), ("20270102", true));
        let week = end("DTSTART:20260228T100000Z", "DURATION:P1W").unwrap();
        assert_eq!(week.raw, "20260307T100000Z");
        // an all-day start with a sub-day duration, or garbage, gives no end
        assert!(end("DTSTART;VALUE=DATE:20261231", "DURATION:PT30M").is_none());
        assert!(end("DTSTART:20260925T100000Z", "DURATION:-PT1H").is_none());
        assert!(end("DTSTART:20260925T100000Z", "DURATION:nonsense").is_none());
        // DTEND wins when both are present
        let both = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:d@x\r\nDTSTART:20260925T100000Z\r\nDTEND:20260925T110000Z\r\nDURATION:PT5H\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(
            parse_calendar_invite(both).unwrap().dtend.unwrap().raw,
            "20260925T110000Z"
        );
    }

    #[test]
    fn mailto_scheme_is_matched_case_insensitively_without_panicking() {
        for a in [
            "mailto:a@x.test",
            "MAILTO:a@x.test",
            "Mailto:a@x.test",
            "mAiLtO:a@x.test",
        ] {
            assert_eq!(strip_mailto(a), "a@x.test");
        }
        assert_eq!(strip_mailto("a@x.test"), "a@x.test");
        assert_eq!(strip_mailto("é"), "é"); // a short, non-ASCII value must not slice mid-char
    }

    #[test]
    fn a_reply_to_a_tzid_invite_has_dtstamp_and_no_dangling_tzid() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:t@x\r\nDTSTART;TZID=Europe/London:20260925T140000\r\nDTEND;TZID=Europe/London:20260925T150000\r\nORGANIZER:mailto:o@x.test\r\nATTENDEE:mailto:me@x.test\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let invite = parse_calendar_invite(ics).unwrap();
        let reply = compose_reply_ics(&invite, "accepted", "me@x.test", None).unwrap();
        assert!(reply.contains("DTSTAMP:"), "{reply}");
        assert!(!reply.contains("TZID"), "{reply}");
        assert!(reply.contains("UID:t@x"));
    }

    #[test]
    fn hostile_duration_inputs_never_panic_or_overflow() {
        let mk = |start: &str, dur: &str| {
            format!(
                "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:h@x\r\n{start}\r\nDURATION:{dur}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
            )
        };
        for (start, dur) in [
            ("DTSTART:99999999T000000Z", "PT1H"),
            ("DTSTART:20260925T990000Z", "PT1H"),
            ("DTSTART:20260925T006199Z", "PT1H"),
            ("DTSTART:99991231T235959Z", "P99999999W"),
            ("DTSTART:20260925T100000Z", "P99999999999999999999W"),
            ("DTSTART:20260925T100000Z", "P9223372036854775807D"),
            ("DTSTART:20260925T100000Z", "PT9223372036854775807S"),
            ("DTSTART:00000101T000000Z", "P1D"),
            ("DTSTART:99991231T235959Z", "PT1S"),
        ] {
            let invite = parse_calendar_invite(&mk(start, dur));
            let end = invite.and_then(|i| i.dtend);
            assert!(
                end.is_none(),
                "{start} + {dur} must be unparseable, got {end:?}"
            );
        }
    }
}
