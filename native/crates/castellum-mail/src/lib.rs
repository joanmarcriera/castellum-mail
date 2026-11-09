// Castellum - RFC822/MIME parsing into normalized envelope + body.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Turns raw message bytes (as fetched from IMAP) into a `ParsedMessage` the sync
// engine maps onto `castellum_models::NewMessage`. Pure and side-effect free.

mod code_highlight;
mod compose_markdown;
pub mod flowed;
mod sanitize;
mod tracker_report;
pub use compose_markdown::{
    is_safe_content_id, referenced_cids, render_markdown_to_html, render_markdown_to_html_themed,
    render_markdown_to_html_with_images, MarkdownTheme, INLINE_IMG_STYLE,
};
pub use sanitize::{sanitize_email_html, SanitizedHtml};

mod trust;
pub use trust::extract_trust_signals;

pub mod headers;
pub use castellum_models::StoredHeader;
pub use headers::extract_stored_headers;

mod link_trust;
pub use link_trust::{inspect_link, LinkInspection};
pub use tracker_report::{
    analyze_remote_content, tracker_service_for_host, HostCount, RemoteContentReport,
    TrackerService,
};

pub mod calendar;
pub use calendar::{parse_calendar_invite, parse_calendar_invite_checked, CalendarParseError};

use castellum_models::{Address, AttachmentDescriptor, CalendarInvite};
use mail_parser::{
    Addr as MpAddr, Address as MpAddress, HeaderName, HeaderValue, MessageParser, MessagePart,
    MimeHeaders, PartType,
};
use std::collections::HashMap;

/// Max characters retained for the preview snippet.
const SNIPPET_LEN: usize = 200;

/// Decode a MIME part body that was fetched raw (IMAP `BODY[<section>]` returns
/// the part still in its `Content-Transfer-Encoding`) into the bytes the sender
/// attached. `encoding` is the lower-cased header value from
/// `AttachmentDescriptor::transfer_encoding`; `None`, `7bit`, `8bit`, `binary`
/// and any unrecognised token are the identity (RFC 2045 §6.4 says to treat an
/// unknown encoding as opaque). Returns `None` when the data is malformed for
/// its declared encoding, so a caller can refuse rather than cache garbage.
pub fn decode_transfer_encoding(raw: &[u8], encoding: Option<&str>) -> Option<Vec<u8>> {
    match encoding.map(|e| e.trim().to_ascii_lowercase()).as_deref() {
        Some("base64") => mail_parser::decoders::base64::base64_decode(raw),
        // mail-parser's strict decoder refuses (or silently truncates at) a
        // stray `=`, which hand-rolled mailers emit; use the lenient one.
        Some("quoted-printable") => Some(lenient_quoted_printable(raw)),
        _ => Some(raw.to_vec()),
    }
}

/// Quoted-printable decode that never fails: `=XX` is a byte, `=` + line break
/// is a soft break, and any other `=` is kept literally.
fn lenient_quoted_printable(raw: &[u8]) -> Vec<u8> {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(raw.len());
    // Length of the run of RAW (not =XX-decoded) space/tab at the end of
    // `out`; RFC 2045 says such transport padding before a hard break is
    // not data, while an encoded `=20` is.
    let mut raw_ws = 0usize;
    let mut i = 0;
    while i < raw.len() {
        let b = raw[i];
        if b == b'\n' || (b == b'\r' && raw.get(i + 1) == Some(&b'\n')) {
            out.truncate(out.len() - raw_ws);
            raw_ws = 0;
            if b == b'\r' {
                out.push(b'\r');
                i += 1;
            }
            out.push(b'\n');
            i += 1;
        } else if b != b'=' {
            raw_ws = if matches!(b, b' ' | b'\t') {
                raw_ws + 1
            } else {
                0
            };
            out.push(b);
            i += 1;
        } else {
            // Soft break: '=' + optional space/tab + CRLF/LF.
            let mut j = i + 1;
            while matches!(raw.get(j), Some(b' ' | b'\t')) {
                j += 1;
            }
            if raw[j..].starts_with(b"\r\n") {
                i = j + 2;
            } else if raw[j..].starts_with(b"\n") {
                i = j + 1;
            } else if let (Some(h), Some(l)) = (
                raw.get(i + 1).and_then(|&b| hex(b)),
                raw.get(i + 2).and_then(|&b| hex(b)),
            ) {
                out.push(h << 4 | l);
                raw_ws = 0;
                i += 3;
            } else {
                out.push(b'=');
                raw_ws = 0;
                i += 1;
            }
        }
    }
    out
}

/// Compute IMAP BODYSTRUCTURE-style dot-separated section paths ("1", "1.2",
/// ...) for every leaf (non-multipart) part in `message`, keyed by
/// mail-parser's internal part id (`message.parts` index). Mirrors RFC 3501
/// §6.4.5 numbering: depth-first, 1-based per nesting level. Does not
/// special-case `multipart/digest` — see the plan's "Assumptions to
/// confirm" #1 for why that is an accepted, narrow simplification.
fn section_paths(message: &mail_parser::Message<'_>) -> HashMap<u32, String> {
    // Iterative, not recursive: mail-parser itself walks MIME nesting with an
    // explicit stack and has no depth limit, so a recursive walk let a few
    // hundred KB of nested `multipart/*` overflow the thread stack — a
    // process abort that no `catch_unwind` can catch. Leaves nested deeper
    // than MAX_SECTION_DEPTH get no path (the same empty `section` an
    // unnumbered part already gets), which also bounds the path strings.
    let mut out = HashMap::new();
    let mut stack: Vec<(u32, String, usize)> = vec![(0, String::new(), 0)];
    while let Some((part_id, prefix, depth)) = stack.pop() {
        let Some(part) = message.parts.get(part_id as usize) else {
            continue;
        };
        if let PartType::Multipart(children) = &part.body {
            if depth >= MAX_SECTION_DEPTH {
                continue;
            }
            for (i, &child_id) in children.iter().enumerate().rev() {
                let path = if prefix.is_empty() {
                    (i + 1).to_string()
                } else {
                    format!("{prefix}.{}", i + 1)
                };
                stack.push((child_id, path, depth + 1));
            }
        } else {
            let path = if prefix.is_empty() {
                "1".to_string()
            } else {
                prefix
            };
            out.insert(part_id, path);
        }
    }
    out
}

/// Deepest `multipart/*` nesting [`section_paths`] numbers. Real mail nests a
/// handful of levels (mixed > alternative > related); anything past this is
/// hostile or broken.
const MAX_SECTION_DEPTH: usize = 64;

/// Normalized parse result for one message. All fields are best-effort.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ParsedMessage {
    pub subject: Option<String>,
    pub from_name: Option<String>,
    pub from_address: Option<String>,
    /// All participants (from/to/cc/bcc/reply-to), in header order.
    pub addresses: Vec<Address>,
    pub rfc822_message_id: Option<String>,
    /// Send time as Unix epoch seconds (from the Date header) if parseable.
    pub sent_at: Option<i64>,
    /// Short preview derived from the text body.
    pub snippet: Option<String>,
    pub text_body: Option<String>,
    /// Raw HTML part, unsanitized. `None` for plain-text-only mail. Kept so a
    /// future sanitizer-policy upgrade can re-sanitize without an IMAP refetch.
    pub html_body: Option<String>,
    pub has_attachments: bool,
    /// Authoritative metadata extracted from the hydrated full MIME body.
    pub attachments: Vec<AttachmentDescriptor>,
    /// Message-ids this message references for threading: In-Reply-To first, then
    /// References, bracket-free, de-duplicated, preserving order.
    pub references: Vec<String>,
    /// First `mailto:` target from List-Unsubscribe (RFC 2369), if any.
    pub list_unsubscribe_mailto: Option<String>,
    /// First `https:` target from List-Unsubscribe (RFC 2369). A bare
    /// `http:` target is never captured here — see `list_unsubscribe_targets`.
    pub list_unsubscribe_url: Option<String>,
    /// True only when `list_unsubscribe_url` is set AND List-Unsubscribe-Post
    /// says exactly `List-Unsubscribe=One-Click` (RFC 8058 §3.1).
    pub list_unsubscribe_one_click: bool,
    /// The first `text/calendar` part's parsed invite (S55), inline or
    /// attached, `None` when absent OR when present but unparseable (a
    /// hostile/malformed `.ics` degrades to no invite card, never a panic
    /// and never a dropped message — see `calendar::parse_calendar_invite`).
    pub calendar_invite: Option<CalendarInvite>,
}

/// A `text/plain; format=flowed` part (RFC 3676) as logical paragraphs; any
/// other text part, and a flowed claim on a non-plain subtype, verbatim. A
/// missing or malformed parameter takes the old path.
fn unflow_if_flowed(part: &MessagePart<'_>, text: &str) -> String {
    let Some(ct) = part.content_type() else {
        return text.to_owned();
    };
    let is_plain = ct.ctype().eq_ignore_ascii_case("text")
        && ct
            .subtype()
            .is_some_and(|s| s.eq_ignore_ascii_case("plain"));
    let flowed = ct
        .attribute("format")
        .is_some_and(|v| v.eq_ignore_ascii_case("flowed"));
    if !(is_plain && flowed) {
        return text.to_owned();
    }
    let delsp = ct
        .attribute("delsp")
        .is_some_and(|v| v.eq_ignore_ascii_case("yes"));
    flowed::decode(text, delsp)
}

/// Parse raw RFC822 bytes into a [`ParsedMessage`]. Never panics; returns an
/// empty result if the bytes cannot be parsed.
pub fn parse_message(raw: &[u8]) -> ParsedMessage {
    parse_message_checked(raw).unwrap_or_default()
}

/// Parse raw RFC822 bytes into a [`ParsedMessage`], or `None` when
/// `mail-parser` cannot make sense of the bytes at all (empty input,
/// binary garbage with no header/body shape). Never panics. IMAP sync
/// callers use [`parse_message`] instead — a descriptor-only stub is
/// harmless mid-sync (the next hydration pass retries). A caller that must
/// instead REFUSE unparseable input outright (S49's `.eml` import: a
/// hostile/corrupt local file is a user-facing error, not a silently-empty
/// imported message) uses this function.
pub fn parse_message_checked(raw: &[u8]) -> Option<ParsedMessage> {
    let message = MessageParser::default().parse(raw)?;

    let subject = message.subject().map(str::to_owned);

    // First From address supplies the top-level from_name/from_address.
    let (from_name, from_address) = match message.from().and_then(MpAddress::first) {
        Some(addr) => (
            addr.name().map(str::to_owned),
            addr.address().map(str::to_owned),
        ),
        None => (None, None),
    };

    // Collect participants in header order: From, To, Cc, Bcc, Reply-To.
    let mut addresses = Vec::new();
    for (kind, header) in [
        ("from", message.from()),
        ("to", message.to()),
        ("cc", message.cc()),
        ("bcc", message.bcc()),
        ("reply-to", message.reply_to()),
    ] {
        let Some(header) = header else { continue };
        for addr in header.iter() {
            // Skip entries with no email address (e.g. bare group names).
            let Some(email) = addr.address() else {
                continue;
            };
            addresses.push(Address {
                kind: kind.to_owned(),
                name: addr.name().map(str::to_owned),
                address: email.to_owned(),
            });
        }
    }

    let rfc822_message_id = message.message_id().map(str::to_owned);

    let sent_at = message.date().map(|dt| dt.to_timestamp());

    // Prefer the plain-text body; fall back to an HTML body stripped to text.
    // When that body is HTML, flatten it ourselves so block boundaries
    // survive (mail-parser only breaks on <br> and </p>).
    // Every inline text part counts (multipart/mixed [plain, plain footer],
    // Mailman digests...), not just the first: join them with a blank line.
    let text_parts: Vec<String> = message
        .text_body
        .iter()
        .filter_map(|id| message.parts.get(*id as usize))
        .filter_map(|part| match &part.body {
            PartType::Text(t) => Some(unflow_if_flowed(part, t.as_ref())),
            PartType::Html(h) => Some(html_to_plain_text(h)),
            _ => None,
        })
        .filter(|t| !t.trim().is_empty())
        .collect();
    let text_body = if text_parts.is_empty() {
        message.body_html(0).map(|html| html_to_plain_text(&html))
    } else {
        Some(text_parts.join("\n\n"))
    }
    .filter(|body| !body.is_empty());

    let snippet = text_body.as_deref().map(make_snippet);

    // Only a REAL html part counts — mail-parser synthesizes HTML from text
    // parts too (PartType::Text in the html_body index), which we must not
    // store or sanitize. We inspect the raw part type to skip synthesized HTML.
    // Real HTML parts only; a plain part sitting beside them (note before an
    // HTML part in multipart/mixed, a footer) is kept as an escaped <pre>.
    let has_real_html = message
        .html_body
        .iter()
        .filter_map(|id| message.parts.get(*id as usize))
        .any(|part| matches!(&part.body, PartType::Html(h) if !h.is_empty()));
    let html_body = has_real_html.then(|| {
        message
            .html_body
            .iter()
            .filter_map(|id| message.parts.get(*id as usize))
            .filter_map(|part| match &part.body {
                PartType::Html(h) if !h.is_empty() => Some(h.as_ref().to_owned()),
                PartType::Text(t) if !t.trim().is_empty() => Some(format!(
                    "<pre style=\"white-space:pre-wrap\">{}</pre>",
                    t.replace('&', "&amp;")
                        .replace('<', "&lt;")
                        .replace('>', "&gt;")
                )),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    });

    let paths = section_paths(&message);
    let attachments = message
        .attachments
        .iter()
        .enumerate()
        .map(|(ordinal, &part_id)| {
            let part = &message.parts[part_id as usize];
            let content_type = part.content_type().and_then(|ct| {
                let primary = ct.c_type.trim();
                if primary.is_empty() {
                    return None;
                }
                Some(match ct.c_subtype.as_deref().map(str::trim) {
                    Some(subtype) if !subtype.is_empty() => {
                        format!(
                            "{}/{}",
                            primary.to_ascii_lowercase(),
                            subtype.to_ascii_lowercase()
                        )
                    }
                    _ => primary.to_ascii_lowercase(),
                })
            });
            let is_inline = part
                .content_disposition()
                .is_some_and(|cd| cd.c_type.eq_ignore_ascii_case("inline"));
            AttachmentDescriptor {
                ordinal: ordinal as i64,
                filename: part
                    .attachment_name()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned),
                content_type,
                size_bytes: i64::try_from(part.len()).ok(),
                section: paths.get(&part_id).cloned().unwrap_or_default(),
                content_id: part.content_id().map(str::to_owned),
                is_inline,
                transfer_encoding: part
                    .content_transfer_encoding()
                    .map(|e| e.trim().to_ascii_lowercase())
                    .filter(|e| !e.is_empty()),
            }
        })
        .collect::<Vec<_>>();
    // Inline cid images (multipart/related) are body furniture, not
    // attachments: same rule as the IMAP envelope path.
    let has_attachments = attachments.iter().any(|a| a.is_real_attachment());

    // Threading references: In-Reply-To first, then References, bracket-free and
    // de-duplicated (first occurrence wins) while preserving order. Both getters
    // return a `HeaderValue`; `as_text_list` normalizes the single-id (`Text`)
    // and multi-id (`TextList`) shapes into one slice, and is `None` for any
    // other/absent shape (contributing nothing).
    let mut references: Vec<String> = Vec::new();
    for header in [message.in_reply_to(), message.references()] {
        let Some(ids) = header.as_text_list() else {
            continue;
        };
        for id in ids {
            // mail-parser already strips angle brackets for id headers; trim
            // defensively for broken-client fallbacks and stray whitespace.
            let id = id.trim().trim_matches(|c| c == '<' || c == '>').trim();
            if id.is_empty() || references.iter().any(|existing| existing == id) {
                continue;
            }
            references.push(id.to_owned());
        }
    }

    let (list_unsubscribe_mailto, list_unsubscribe_url, list_unsubscribe_one_click) =
        list_unsubscribe_targets(&message);

    // A calendar-parser panic must never take mail parsing down with it —
    // wrapped the same way the HTML sanitizer and trust extractor are
    // wrapped at the sync layer (see `castellum_core::sync::new_message_from_raw`).
    let calendar_invite = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        calendar::find_calendar_invite(&message)
    }))
    .unwrap_or(None);

    Some(ParsedMessage {
        subject,
        from_name,
        from_address,
        addresses,
        rfc822_message_id,
        sent_at,
        snippet,
        text_body,
        html_body,
        has_attachments,
        attachments,
        references,
        list_unsubscribe_mailto,
        list_unsubscribe_url,
        list_unsubscribe_one_click,
        calendar_invite,
    })
}

/// Extract RFC 2369 `List-Unsubscribe` targets (first `mailto:` and first
/// `https:` URI, in header order) and whether the message opts into RFC 8058
/// one-click (`List-Unsubscribe-Post: List-Unsubscribe=One-Click`, which only
/// ever applies to the `https:` target — RFC 8058 requires TLS, so a bare
/// `http:` entry is never captured as a URL at all and can never be
/// one-click by construction). Never panics: an absent header, a malformed
/// one, an address-`Group`-shaped one (RFC 2369 never groups; some senders
/// still send garbage), or an entry with no address, all just contribute
/// nothing.
fn list_unsubscribe_targets(
    message: &mail_parser::Message<'_>,
) -> (Option<String>, Option<String>, bool) {
    let mut mailto = None;
    let mut http_url = None;
    if let HeaderValue::Address(address) = message.list_unsubscribe() {
        let addrs: &[MpAddr] = match address {
            MpAddress::List(list) => list,
            MpAddress::Group(groups) => groups
                .first()
                .map(|g| g.addresses.as_slice())
                .unwrap_or(&[]),
        };
        for a in addrs {
            let Some(uri) = a.address.as_deref() else {
                continue;
            };
            let lower = uri.trim().to_ascii_lowercase();
            if mailto.is_none() && lower.starts_with("mailto:") {
                mailto = Some(uri.trim().to_owned());
            } else if http_url.is_none() && lower.starts_with("https://") {
                http_url = Some(uri.trim().to_owned());
            }
        }
    }
    let one_click = http_url.is_some()
        && matches!(
            message.header(HeaderName::ListUnsubscribePost),
            Some(HeaderValue::Text(s)) if s.trim().eq_ignore_ascii_case("List-Unsubscribe=One-Click")
        );
    (mailto, http_url, one_click)
}

/// HTML to plain text that keeps block boundaries: a `<br>` goes in before the
/// closing tag of every block-level element and a space after `</td>`/`</th>`,
/// then mail-parser's decoder handles entities, `<style>` and the rest. Runs of
/// three or more newlines collapse to one blank line. Only affects bodies
/// parsed from now on (no backfill of stored bodies).
fn html_to_plain_text(html: &str) -> String {
    const BLOCKS: [&str; 16] = [
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "div",
        "li",
        "tr",
        "blockquote",
        "table",
        "ul",
        "ol",
        "dt",
        "dd",
        "pre",
    ];
    let mut prepared = String::with_capacity(html.len() + html.len() / 8);
    let mut rest = html;
    while let Some(pos) = rest.find("</") {
        prepared.push_str(&rest[..pos]);
        let tail = &rest[pos + 2..];
        let name_len = tail
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(tail.len());
        let name = &tail[..name_len];
        if BLOCKS.iter().any(|b| name.eq_ignore_ascii_case(b)) {
            prepared.push_str("<br>");
        }
        let cell = name.eq_ignore_ascii_case("td") || name.eq_ignore_ascii_case("th");
        match tail.find('>') {
            Some(end) => {
                prepared.push_str(&rest[pos..pos + 2 + end + 1]);
                rest = &tail[end + 1..];
            }
            None => {
                prepared.push_str(&rest[pos..]);
                rest = "";
            }
        }
        if cell {
            prepared.push(' ');
        }
    }
    prepared.push_str(rest);
    let text = mail_parser::decoders::html::html_to_text(&prepared);
    let mut out = String::with_capacity(text.len());
    let mut newlines = 0;
    for ch in text.chars() {
        if ch == '\n' {
            newlines += 1;
            if newlines > 2 {
                continue;
            }
        } else {
            newlines = 0;
        }
        out.push(ch);
    }
    out
}

/// First ~200 chars of the body, whitespace-collapsed with no newlines.
fn make_snippet(body: &str) -> String {
    let collapsed = body.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(SNIPPET_LEN).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_text_email() {
        let raw = b"From: Alice Example <alice@example.com>\r\n\
To: Bob <bob@example.org>\r\n\
Subject: Hello there\r\n\
Message-ID: <abc123@example.com>\r\n\
Date: Tue, 1 Jul 2025 10:00:00 +0000\r\n\
\r\n\
This is the body of the message.\r\n";

        let parsed = parse_message(raw);

        assert_eq!(parsed.subject.as_deref(), Some("Hello there"));
        assert_eq!(parsed.from_address.as_deref(), Some("alice@example.com"));
        assert_eq!(parsed.from_name.as_deref(), Some("Alice Example"));
        assert_eq!(
            parsed.rfc822_message_id.as_deref(),
            Some("abc123@example.com")
        );
        assert!(parsed.sent_at.is_some_and(|ts| ts > 0));
        assert!(parsed
            .text_body
            .as_deref()
            .is_some_and(|b| b.contains("body of the message")));
        assert_eq!(
            parsed.snippet.as_deref(),
            Some("This is the body of the message.")
        );
        assert!(!parsed.has_attachments);

        // A "to" address with the right kind must be present.
        assert!(parsed
            .addresses
            .iter()
            .any(|a| a.kind == "to" && a.address == "bob@example.org"));
    }

    #[test]
    fn parse_message_checked_returns_none_for_empty_input() {
        assert!(parse_message_checked(b"").is_none());
    }

    #[test]
    fn parse_message_checked_treats_pure_garbage_as_an_empty_parsed_message() {
        // Deviation from the original plan (S49): mail-parser 0.11.8's
        // `parse()` only returns `None` for byte slices with NO header
        // shape at all found (verified against
        // mail-parser-0.11.8/src/parsers/message.rs's `else { None }`
        // branch, reached only when `part_headers` stays empty). A run of
        // non-whitespace high bytes with no ':' or '\n' still yields one
        // synthetic `HeaderName::Other` header (see `parse_header_name`'s
        // EOF fallback), so mail-parser hands back `Some(Message)` — just
        // one whose recognised fields (subject/from/date/body/...) are all
        // empty. `parse_message_checked` therefore returns
        // `Some(ParsedMessage::default())` here, not `None`. Callers that
        // need to refuse genuinely garbage input (S49's `.eml` import)
        // additionally check `parsed == ParsedMessage::default()` — see
        // `import_eml.rs`.
        let garbage = vec![0xFF_u8; 64];
        let parsed = parse_message_checked(&garbage);
        assert_eq!(parsed, Some(ParsedMessage::default()));
    }

    #[test]
    fn parse_message_checked_matches_parse_message_on_valid_input() {
        let raw = b"From: a@x\r\nSubject: hi\r\n\r\nbody\r\n";
        let checked = parse_message_checked(raw).expect("valid message");
        assert_eq!(parse_message(raw), checked);
    }

    #[test]
    fn parse_message_still_never_panics_on_bytes_the_checked_variant_refuses() {
        // parse_message's existing IMAP-sync contract: never panic, always
        // return a (possibly default) ParsedMessage.
        assert_eq!(parse_message(b""), ParsedMessage::default());
    }

    #[test]
    fn collects_cc_and_multiple_to_in_order() {
        let raw = b"From: sender@example.com\r\n\
To: one@example.com, two@example.com\r\n\
Cc: carbon@example.com\r\n\
Subject: Multi\r\n\
Date: Tue, 1 Jul 2025 10:00:00 +0000\r\n\
\r\n\
Body.\r\n";

        let parsed = parse_message(raw);

        let kinds: Vec<&str> = parsed.addresses.iter().map(|a| a.kind.as_str()).collect();
        let emails: Vec<&str> = parsed
            .addresses
            .iter()
            .map(|a| a.address.as_str())
            .collect();

        // Order must be From, To (in list order), then Cc.
        assert_eq!(kinds, vec!["from", "to", "to", "cc"]);
        assert_eq!(
            emails,
            vec![
                "sender@example.com",
                "one@example.com",
                "two@example.com",
                "carbon@example.com",
            ]
        );
    }

    #[test]
    fn detects_attachment_and_extracts_text() {
        let raw = b"From: sender@example.com\r\n\
To: rcpt@example.com\r\n\
Subject: With attachment\r\n\
Date: Tue, 1 Jul 2025 10:00:00 +0000\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"BOUNDARY\"\r\n\
\r\n\
--BOUNDARY\r\n\
Content-Type: text/plain; charset=\"utf-8\"\r\n\
\r\n\
Please see the attached file.\r\n\
--BOUNDARY\r\n\
Content-Type: application/octet-stream; name=\"data.bin\"\r\n\
Content-Disposition: attachment; filename=\"data.bin\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
aGVsbG8gd29ybGQ=\r\n\
--BOUNDARY--\r\n";

        let parsed = parse_message(raw);

        assert!(parsed.has_attachments);
        assert_eq!(parsed.attachments.len(), 1);
        assert_eq!(parsed.attachments[0].ordinal, 0);
        assert_eq!(parsed.attachments[0].filename.as_deref(), Some("data.bin"));
        assert_eq!(
            parsed.attachments[0].content_type.as_deref(),
            Some("application/octet-stream")
        );
        assert_eq!(parsed.attachments[0].size_bytes, Some(11));
        assert!(parsed
            .text_body
            .as_deref()
            .is_some_and(|b| b.contains("attached file")));
    }

    #[test]
    fn nested_multipart_attachments_get_imap_section_paths() {
        // multipart/mixed
        //   1: text/plain
        //   2: multipart/related
        //        2.1: text/html
        //        2.2: image/png (inline, Content-ID)
        //   3: application/pdf (regular attachment)
        let raw = b"From: sender@example.com\r\n\
To: rcpt@example.com\r\n\
Subject: Nested\r\n\
Date: Tue, 1 Jul 2025 10:00:00 +0000\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"OUTER\"\r\n\
\r\n\
--OUTER\r\n\
Content-Type: text/plain; charset=\"utf-8\"\r\n\
\r\n\
Plain part.\r\n\
--OUTER\r\n\
Content-Type: multipart/related; boundary=\"INNER\"\r\n\
\r\n\
--INNER\r\n\
Content-Type: text/html; charset=\"utf-8\"\r\n\
\r\n\
<p>HTML with <img src=\"cid:logo123\"></p>\r\n\
--INNER\r\n\
Content-Type: image/png\r\n\
Content-ID: <logo123>\r\n\
Content-Disposition: inline; filename=\"logo.png\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
aGVsbG8=\r\n\
--INNER--\r\n\
--OUTER\r\n\
Content-Type: application/pdf; name=\"report.pdf\"\r\n\
Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
d29ybGQ=\r\n\
--OUTER--\r\n";

        let parsed = parse_message(raw);

        assert_eq!(parsed.attachments.len(), 2, "the inline image and the PDF");

        let inline = parsed
            .attachments
            .iter()
            .find(|a| a.filename.as_deref() == Some("logo.png"))
            .expect("inline image attachment");
        assert_eq!(inline.section, "2.2");
        assert_eq!(inline.content_id.as_deref(), Some("logo123"));
        assert!(inline.is_inline);

        let pdf = parsed
            .attachments
            .iter()
            .find(|a| a.filename.as_deref() == Some("report.pdf"))
            .expect("pdf attachment");
        assert_eq!(pdf.section, "3");
        assert_eq!(pdf.content_id, None);
        assert!(!pdf.is_inline);
    }

    #[test]
    fn attachment_descriptor_records_the_declared_transfer_encoding() {
        let raw = b"From: a@example.com\r\n\
To: b@example.com\r\n\
Subject: Encodings\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"B\"\r\n\
\r\n\
--B\r\n\
Content-Type: text/plain\r\n\
\r\n\
body\r\n\
--B\r\n\
Content-Type: application/pdf; name=\"a.pdf\"\r\n\
Content-Disposition: attachment; filename=\"a.pdf\"\r\n\
Content-Transfer-Encoding: BASE64\r\n\
\r\n\
aGVsbG8=\r\n\
--B\r\n\
Content-Type: application/octet-stream; name=\"b.bin\"\r\n\
Content-Disposition: attachment; filename=\"b.bin\"\r\n\
\r\n\
raw bytes\r\n\
--B--\r\n";
        let parsed = parse_message(raw);
        assert_eq!(parsed.attachments.len(), 2);
        assert_eq!(
            parsed.attachments[0].transfer_encoding.as_deref(),
            Some("base64"),
            "lower-cased"
        );
        assert_eq!(parsed.attachments[1].transfer_encoding, None);
    }

    #[test]
    fn decode_transfer_encoding_handles_each_encoding() {
        // base64 with the line wrapping real mailers emit
        assert_eq!(
            decode_transfer_encoding(b"aGVsbG8g\r\nd29ybGQ=\r\n", Some("base64")),
            Some(b"hello world".to_vec())
        );
        assert_eq!(
            decode_transfer_encoding(b"caf=C3=A9 =\r\nbar", Some("quoted-printable")),
            Some("caf\u{e9} bar".as_bytes().to_vec())
        );
        // Identity encodings and unknown tokens pass through untouched.
        for enc in [
            None,
            Some("7bit"),
            Some("8bit"),
            Some("binary"),
            Some("x-uuencode"),
        ] {
            assert_eq!(
                decode_transfer_encoding(b"as is", enc),
                Some(b"as is".to_vec())
            );
        }
        // Case/whitespace in the token are tolerated.
        assert_eq!(
            decode_transfer_encoding(b"aGk=", Some(" Base64 ")),
            Some(b"hi".to_vec())
        );
        // Malformed data for its declared encoding is refused, not garbled.
        assert_eq!(
            decode_transfer_encoding(b"!!not base64!!", Some("base64")),
            None
        );
    }

    #[test]
    fn single_part_attachment_message_has_section_one() {
        // A non-multipart message whose entire body IS the attachment
        // (unusual but legal) addresses as IMAP section "1".
        let raw = b"From: sender@example.com\r\n\
To: rcpt@example.com\r\n\
Subject: Just a file\r\n\
Date: Tue, 1 Jul 2025 10:00:00 +0000\r\n\
Content-Type: application/pdf; name=\"solo.pdf\"\r\n\
Content-Disposition: attachment; filename=\"solo.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
aGVsbG8=\r\n";

        let parsed = parse_message(raw);
        assert_eq!(parsed.attachments.len(), 1);
        assert_eq!(parsed.attachments[0].section, "1");
    }

    #[test]
    fn references_in_reply_to_first_then_references_deduped() {
        let raw = b"From: sender@example.com\r\n\
To: rcpt@example.com\r\n\
Subject: Re: Hello\r\n\
In-Reply-To: <parent@x>\r\n\
References: <root@x> <parent@x>\r\n\
Date: Tue, 1 Jul 2025 10:00:00 +0000\r\n\
\r\n\
Reply body.\r\n";

        let parsed = parse_message(raw);

        // In-Reply-To first, then References; `parent@x` not repeated.
        assert_eq!(parsed.references, vec!["parent@x", "root@x"]);
    }

    #[test]
    fn references_absent_yields_empty() {
        let raw = b"From: sender@example.com\r\n\
To: rcpt@example.com\r\n\
Subject: No threading\r\n\
Date: Tue, 1 Jul 2025 10:00:00 +0000\r\n\
\r\n\
Body.\r\n";

        let parsed = parse_message(raw);

        assert!(parsed.references.is_empty());
    }

    #[test]
    fn references_only_multiple_ids_in_order_bracket_free() {
        let raw = b"From: sender@example.com\r\n\
To: rcpt@example.com\r\n\
Subject: Deep thread\r\n\
References: <a@x> <b@x> <c@x>\r\n\
Date: Tue, 1 Jul 2025 10:00:00 +0000\r\n\
\r\n\
Body.\r\n";

        let parsed = parse_message(raw);

        assert_eq!(parsed.references, vec!["a@x", "b@x", "c@x"]);
    }

    #[test]
    fn malformed_input_returns_default_without_panicking() {
        let parsed = parse_message(b"");
        assert_eq!(parsed, ParsedMessage::default());

        // Garbage bytes must not panic; result is best-effort/empty-ish.
        let _ = parse_message(b"\x00\x01\x02 not an email at all");
    }

    #[test]
    fn extracts_raw_html_body_when_present() {
        let raw = b"From: a@b\r\nTo: c@d\r\nSubject: h\r\nMIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Hello <b>HTML</b></p>";
        let parsed = parse_message(raw);
        let html = parsed.html_body.expect("html body");
        assert!(html.contains("<b>HTML</b>"));
        // text fallback still derived from HTML for search/snippets
        assert!(parsed.text_body.expect("text").contains("HTML"));
    }

    #[test]
    fn plain_text_mail_has_no_html_body() {
        let raw = b"From: a@b\r\nTo: c@d\r\nSubject: t\r\n\r\njust text";
        let parsed = parse_message(raw);
        assert!(parsed.html_body.is_none());
        assert_eq!(parsed.text_body.as_deref(), Some("just text"));
    }

    // --- List-Unsubscribe (S44) ---------------------------------------------

    fn parse(raw: &str) -> ParsedMessage {
        parse_message(raw.as_bytes())
    }

    #[test]
    fn extracts_mailto_and_https_targets_from_list_unsubscribe() {
        let raw = "From: a@b.com\r\n\
                    Subject: newsletter\r\n\
                    List-Unsubscribe: <mailto:unsub@example.com?subject=unsubscribe>, <https://example.com/unsub?id=42>\r\n\
                    List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n\
                    \r\n\
                    body\r\n";
        let p = parse(raw);
        assert_eq!(
            p.list_unsubscribe_mailto.as_deref(),
            Some("mailto:unsub@example.com?subject=unsubscribe")
        );
        assert_eq!(
            p.list_unsubscribe_url.as_deref(),
            Some("https://example.com/unsub?id=42")
        );
        assert!(p.list_unsubscribe_one_click);
    }

    #[test]
    fn absent_header_yields_none_and_not_one_click() {
        let raw = "From: a@b.com\r\nSubject: plain\r\n\r\nbody\r\n";
        let p = parse(raw);
        assert!(p.list_unsubscribe_mailto.is_none());
        assert!(p.list_unsubscribe_url.is_none());
        assert!(!p.list_unsubscribe_one_click);
    }

    #[test]
    fn a_bare_http_target_is_never_captured_as_the_url_even_with_one_click_post() {
        // RFC 8058 requires TLS; a non-https target must never be treated as
        // one-click-capable, however the sender labels List-Unsubscribe-Post.
        let raw = "From: a@b.com\r\nSubject: s\r\n\
                    List-Unsubscribe: <http://example.com/unsub>\r\n\
                    List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n\
                    \r\nbody\r\n";
        let p = parse(raw);
        assert!(p.list_unsubscribe_url.is_none());
        assert!(!p.list_unsubscribe_one_click);
    }

    #[test]
    fn one_click_requires_both_an_https_target_and_the_exact_post_marker() {
        let raw = "From: a@b.com\r\nSubject: s\r\n\
                    List-Unsubscribe: <https://example.com/unsub>\r\n\
                    \r\nbody\r\n"; // no List-Unsubscribe-Post at all
        let p = parse(raw);
        assert_eq!(
            p.list_unsubscribe_url.as_deref(),
            Some("https://example.com/unsub")
        );
        assert!(!p.list_unsubscribe_one_click);
    }

    #[test]
    fn malformed_list_unsubscribe_header_never_panics() {
        // Hostile/garbled input: no angle brackets, stray commas, empty.
        for raw in [
            "From: a@b.com\r\nList-Unsubscribe: not-a-uri-at-all\r\n\r\nb\r\n",
            "From: a@b.com\r\nList-Unsubscribe: ,,,\r\n\r\nb\r\n",
            "From: a@b.com\r\nList-Unsubscribe: \r\n\r\nb\r\n",
            "From: a@b.com\r\nList-Unsubscribe: <ftp://example.com/x>\r\n\r\nb\r\n",
        ] {
            let _ = parse(raw); // must not panic
        }
    }

    #[test]
    fn only_the_first_mailto_and_first_https_target_are_kept() {
        let raw = "From: a@b.com\r\n\
                    List-Unsubscribe: <mailto:one@x.com>, <mailto:two@x.com>, <https://x.com/a>, <https://x.com/b>\r\n\
                    \r\nbody\r\n";
        let p = parse(raw);
        assert_eq!(
            p.list_unsubscribe_mailto.as_deref(),
            Some("mailto:one@x.com")
        );
        assert_eq!(p.list_unsubscribe_url.as_deref(), Some("https://x.com/a"));
    }

    /// MM-5: a reply quotes `text_body`. For an HTML-only message that comes from
    /// mail-parser's own HTML-to-text conversion; pin that it stays free of CSS and
    /// markup and has entities decoded, so a reply to HTML mail is quoted as clean
    /// plain text (the HTML itself is never quoted).
    #[test]
    fn html_only_message_quotes_as_clean_plain_text() {
        let raw = b"From: a@x.test\r\nTo: b@x.test\r\nSubject: Hi\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<html><head><style>body{font:12px}</style></head><body><p>Dear Bob,</p><p>Lunch &amp; a chat?<br>Ana</p></body></html>\r\n";
        let parsed = parse_message(raw);
        let text = parsed
            .text_body
            .expect("text body derived from the html part");
        assert!(
            text.contains("Dear Bob,") && text.contains("Lunch & a chat?") && text.contains("Ana"),
            "{text:?}"
        );
        for leaked in ["font:12px", "<p>", "</", "&amp;", "<style"] {
            assert!(!text.contains(leaked), "{leaked:?} leaked into {text:?}");
        }
        assert!(!parsed
            .snippet
            .as_deref()
            .unwrap_or("")
            .contains("font:12px"));
    }

    /// Block-level HTML elements must not fuse their words when flattened.
    #[test]
    fn html_only_block_boundaries_stay_separate_words() {
        let cases: [(&str, &str, &str); 5] = [
            ("<h1>Numbers</h1><p>Revenue</p>", "Numbers", "Revenue"),
            ("<div>Alpha</div><div>Beta</div>", "Alpha", "Beta"),
            ("<ul><li>Alpha</li><li>Beta</li></ul>", "Alpha", "Beta"),
            (
                "<table><tr><td>Alpha</td><td>Beta</td></tr></table>",
                "Alpha",
                "Beta",
            ),
            ("<p>Alpha</p><blockquote>Beta</blockquote>", "Alpha", "Beta"),
        ];
        for (html, a, b) in cases {
            let raw = format!(
                "From: a@x.test\r\nTo: b@x.test\r\nSubject: Hi\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<html><body>{html}</body></html>\r\n"
            );
            let text = parse_message(raw.as_bytes()).text_body.expect("text body");
            let fused = format!("{a}{b}");
            assert!(!text.contains(&fused), "{html}: fused in {text:?}");
            assert!(text.contains(a) && text.contains(b), "{html}: {text:?}");
            assert!(!text.contains("\n\n\n"), "{html}: {text:?}");
        }
    }

    #[test]
    fn quoted_printable_with_stray_equals_decodes_leniently() {
        assert_eq!(
            decode_transfer_encoding(b"a=b", Some("quoted-printable")),
            Some(b"a=b".to_vec())
        );
        assert_eq!(
            decode_transfer_encoding(b"x=3Dy =\r\nz=", Some("quoted-printable")),
            Some(b"x=y z=".to_vec())
        );
        let qp = |s: &[u8]| decode_transfer_encoding(s, Some("quoted-printable")).unwrap();
        assert_eq!(qp(b"a=z"), b"a=z");
        assert_eq!(qp(b"x==41"), b"x=A");
        assert_eq!(qp(b"k=v1"), b"k=v1");
        assert_eq!(qp(b"abc="), b"abc=");
        assert_eq!(qp(b"foo=20\r\n"), b"foo \r\n");
        assert_eq!(qp(b"foo=20\n"), b"foo \n");
        assert_eq!(qp(b"foo  \r\n"), b"foo\r\n");
        assert_eq!(qp(b"foo \t\n"), b"foo\n");
        assert_eq!(qp(b"a=  \r\nb"), b"ab");
        assert_eq!(qp(b"a=\t\nb"), b"ab");
    }

    #[test]
    fn html_part_after_plain_part_in_multipart_mixed_is_kept() {
        let raw = b"From: a@example.com\r\nSubject: Gr\xc3\xa0cies\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"B\"\r\n\r\n\
--B\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nHola\r\n\
--B\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Hola <b>m\xc3\xb3n</b></p>\r\n--B--\r\n";
        let parsed = parse_message(raw);
        assert!(parsed
            .html_body
            .as_deref()
            .is_some_and(|h| h.contains("<b>")));
    }

    #[test]
    fn inline_cid_image_is_not_an_attachment_but_a_pdf_is() {
        let related = b"From: a@example.com\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/related; boundary=\"R\"\r\n\r\n\
--R\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<img src=\"cid:logo\">\r\n\
--R\r\nContent-Type: image/png\r\nContent-ID: <logo>\r\nContent-Disposition: inline\r\n\
Content-Transfer-Encoding: base64\r\n\r\naGk=\r\n--R--\r\n";
        let p = parse_message(related);
        assert_eq!(p.attachments.len(), 1, "descriptor kept for cid lookup");
        assert!(!p.has_attachments);
        let pdf = b"From: a@example.com\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"M\"\r\n\r\n\
--M\r\nContent-Type: text/plain\r\n\r\nx\r\n\
--M\r\nContent-Type: application/pdf; name=\"a.pdf\"\r\nContent-Disposition: attachment; filename=\"a.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\r\naGk=\r\n--M--\r\n";
        assert!(parse_message(pdf).has_attachments);
    }

    #[test]
    fn cjk_charsets_decode_in_body_and_subject() {
        // ISO-2022-JP "日本語", GBK "中文", Shift_JIS "日本語"
        let raw = b"From: a@example.com\r\nSubject: =?ISO-2022-JP?B?GyRCRnxLXDhsGyhC?=\r\n\
MIME-Version: 1.0\r\nContent-Type: text/plain; charset=gbk\r\n\
Content-Transfer-Encoding: 8bit\r\n\r\n\xd6\xd0\xce\xc4\r\n";
        let p = parse_message(raw);
        assert_eq!(p.subject.as_deref(), Some("\u{65e5}\u{672c}\u{8a9e}"));
        assert_eq!(
            p.text_body.as_deref().map(str::trim),
            Some("\u{4e2d}\u{6587}")
        );
        let sjis = b"From: a@example.com\r\nContent-Type: text/plain; charset=shift_jis\r\n\r\n\x93\xfa\x96\x7b\x8c\xea";
        assert_eq!(
            parse_message(sjis).text_body.as_deref(),
            Some("\u{65e5}\u{672c}\u{8a9e}")
        );
    }

    #[test]
    fn every_inline_text_part_reaches_text_body_and_html_body() {
        let two_plain = b"From: a@example.com\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"B\"\r\n\r\n\
--B\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nHola m\xc3\xb3n\r\n\
--B\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nPeu de llista\r\n--B--\r\n";
        let p = parse_message(two_plain);
        let t = p.text_body.expect("text");
        assert!(
            t.contains("Hola m\u{f3}n") && t.contains("Peu de llista"),
            "{t:?}"
        );
        assert!(p.html_body.is_none(), "no real html: nothing synthesized");

        let mixed = b"From: a@example.com\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"B\"\r\n\r\n\
--B\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nNota <1>\r\n\
--B\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Cos</p>\r\n--B--\r\n";
        let h = parse_message(mixed).html_body.expect("html");
        assert!(
            h.contains("<p>Cos</p>") && h.contains("Nota &lt;1&gt;"),
            "{h:?}"
        );

        let alt = b"From: a@example.com\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/alternative; boundary=\"B\"\r\n\r\n\
--B\r\nContent-Type: text/plain\r\n\r\nplain\r\n\
--B\r\nContent-Type: text/html\r\n\r\n<p>html</p>\r\n--B--\r\n";
        let p = parse_message(alt);
        assert_eq!(p.text_body.as_deref(), Some("plain"));
        assert_eq!(p.html_body.as_deref(), Some("<p>html</p>"));
    }

    // ---- format=flowed ----

    fn plain_message(content_type: &str, body: &str) -> Vec<u8> {
        format!(
            "From: a@x.test\r\nTo: b@x.test\r\nSubject: t\r\nMessage-ID: <f@x.test>\r\n\
             MIME-Version: 1.0\r\nContent-Type: {content_type}\r\n\r\n{body}"
        )
        .into_bytes()
    }

    #[test]
    fn flowed_plain_text_is_unflowed_at_parse_time() {
        let raw = plain_message(
            "text/plain; charset=utf-8; format=flowed",
            "Hello there, this is a long \r\nparagraph wrapped by the sender.\r\n> quoted a \r\n> quoted b\r\n",
        );
        let parsed = parse_message(&raw);
        let body = parsed.text_body.expect("body");
        assert!(
            body.starts_with("Hello there, this is a long paragraph wrapped by the sender."),
            "{body:?}"
        );
        assert!(body.contains("> quoted a quoted b"), "{body:?}");
        assert!(
            parsed.snippet.unwrap().contains("long paragraph"),
            "snippet is derived from the decoded text"
        );
    }

    #[test]
    fn flowed_delsp_yes_is_honoured() {
        let raw = plain_message(
            "text/plain; charset=utf-8; format=Flowed; DelSp=Yes",
            "ab \r\ncd",
        );
        assert_eq!(parse_message(&raw).text_body.as_deref(), Some("abcd"));
    }

    #[test]
    fn fixed_plain_text_is_untouched() {
        let body = "line one \r\nline two";
        let raw = plain_message("text/plain; charset=utf-8", body);
        assert_eq!(
            parse_message(&raw).text_body.as_deref(),
            Some("line one \r\nline two"),
            "no format=flowed: trailing space stays, lines stay separate"
        );
    }

    #[test]
    fn format_fixed_is_untouched() {
        let raw = plain_message("text/plain; format=fixed", "a \r\nb");
        assert_eq!(parse_message(&raw).text_body.as_deref(), Some("a \r\nb"));
    }

    #[test]
    fn malformed_flowed_parameter_falls_back_to_verbatim() {
        let raw = plain_message("text/plain; format=", "a \r\nb");
        assert_eq!(parse_message(&raw).text_body.as_deref(), Some("a \r\nb"));
    }
}
