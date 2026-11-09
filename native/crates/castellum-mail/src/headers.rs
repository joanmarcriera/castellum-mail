// Castellum - selected message header extraction (MM-1c, #2710).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Usage: `extract_stored_headers(raw)` takes a full RFC822 message or a bare
// header block and returns the headers worth keeping for "any header"
// smart-mailbox conditions: every header except the denylist in
// `castellum_models::header_name_status` (routing/signature noise, message
// identity, MIME structure and anything credential-bearing), capped at
// HEADER_MAX_PER_NAME occurrences per name and HEADER_MAX_PER_MESSAGE rows,
// each value unfolded, RFC 2047-decoded, whitespace-collapsed, trimmed and cut
// at HEADER_VALUE_MAX_BYTES on a char boundary (a cut value ends with
// HEADER_VALUE_CUT_MARKER). Values shaped like an HTTP credential
// (`Bearer ...`, `Basic ...`) are dropped whatever the header name.
//
// Header values are UNTRUSTED mail content: this never panics (mail-parser is
// panic-free by design and callers additionally wrap it in `catch_unwind`),
// only ever scans the top-level header block, bounded by INPUT_CAP_BYTES (the
// body and its attachments are never read), and returns an empty Vec when nothing
// parses. Values never reach logs or diagnostics from here.

use castellum_models::{
    header_name_status, is_credential_like_value, normalize_header_name, HeaderNameStatus,
    StoredHeader, HEADER_MAX_PER_MESSAGE, HEADER_MAX_PER_NAME, HEADER_VALUE_CUT_MARKER,
    HEADER_VALUE_MAX_BYTES,
};
use mail_parser::{HeaderForm, MessageParser};
use std::collections::HashMap;

/// Only the first MiB of the HEADER BLOCK is looked at: a header block larger
/// than that is hostile or broken, and the cap bounds parse cost. The body
/// (attachments included) is never scanned, so a large message is not partial
/// just because it is large.
const INPUT_CAP_BYTES: usize = 1 << 20;

/// The top-level header block of `raw` as mail-parser delimits it: everything
/// up to and including the first EMPTY line (`\r\n` or `\n`). A line of only
/// spaces/tabs is a folded continuation, not the end (it marks the capture
/// partial). Only the first [`INPUT_CAP_BYTES`] are scanned.
struct HeaderBlock {
    /// Bytes of `raw` that belong to the header block (<= INPUT_CAP_BYTES).
    len: usize,
    /// The block may be incomplete: no blank line within the cap while more
    /// input follows, or a non-continuation line without a `:` (mail-parser
    /// skips such a line silently, so a header could vanish unnoticed).
    suspect: bool,
}

fn header_block(raw: &[u8]) -> HeaderBlock {
    let window = &raw[..raw.len().min(INPUT_CAP_BYTES)];
    let mut suspect = false;
    let mut start = 0;
    while start < window.len() {
        let (line_end, next) = match window[start..].iter().position(|&b| b == b'\n') {
            Some(i) => (start + i, start + i + 1),
            // Last line, no newline: it ends the window.
            None => (window.len(), window.len()),
        };
        let line = &window[start..line_end];
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            // Empty line: the header block ends here (mail-parser's rule:
            // only "\r\n" / "\n" ends it).
            return HeaderBlock { len: next, suspect };
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            // A line of only SP/HT is a folded continuation for mail-parser,
            // NOT the end of the block: keep scanning, so headers below it are
            // still captured. Such a line is unusual enough (and was once a
            // way to hide later headers) that the capture is marked partial.
            suspect = true;
            start = next;
            continue;
        }
        let continuation = matches!(line.first(), Some(b' ' | b'\t'));
        if !continuation && !line.contains(&b':') {
            suspect = true;
        }
        start = next;
    }
    // No blank line. A bare header block that fits the cap is complete; one
    // that runs past the cap was cut.
    if raw.len() > INPUT_CAP_BYTES {
        suspect = true;
    }
    HeaderBlock {
        len: window.len(),
        suspect,
    }
}

/// See the module header. Output order: first appearance of each name, then
/// document order within a name.
///
/// Returns `(headers, truncated)`. `truncated` is true whenever the result may
/// be incomplete: the header block exceeded INPUT_CAP_BYTES or held a line
/// that is not a header, the headers could not be parsed at all, a name had
/// more than HEADER_MAX_PER_NAME occurrences, the HEADER_MAX_PER_MESSAGE row
/// cap cut the output, a value was cut at HEADER_VALUE_MAX_BYTES, or a
/// credential-like value was dropped. A caller must store such a result as a
/// PARTIAL capture (unknown, never "this header is absent").
pub fn extract_stored_headers(raw: &[u8]) -> (Vec<StoredHeader>, bool) {
    let block = header_block(raw);
    let mut truncated = block.suspect;
    let raw = &raw[..block.len];
    let Some(message) = MessageParser::default().parse_headers(raw) else {
        return (Vec::new(), true);
    };

    // Non-empty input that yields no header at all did not really parse.
    if message.headers().is_empty() {
        truncated = true;
    }

    // Group by lower-cased name, remembering first-appearance order. Names
    // that are invalid or denylisted are dropped here, before any decoding,
    // so thousands of `Received:` lines cost nothing beyond the scan.
    let mut order: Vec<String> = Vec::new();
    let mut seen: HashMap<String, mail_parser::HeaderName<'_>> = HashMap::new();
    for h in message.headers() {
        let Some(name) = normalize_header_name(h.name.as_str()) else {
            // A header we cannot even name was dropped: the capture is incomplete.
            truncated = true;
            continue;
        };
        if header_name_status(&name) != HeaderNameStatus::Storable || seen.contains_key(&name) {
            continue;
        }
        seen.insert(name.clone(), h.name.clone());
        order.push(name);
    }

    let mut out: Vec<StoredHeader> = Vec::new();
    for name in order {
        if out.len() >= HEADER_MAX_PER_MESSAGE {
            truncated = true;
            break;
        }
        let Some(header_name) = seen.remove(&name) else {
            continue;
        };
        // All occurrences of this name, decoded as unstructured text
        // (unfolds and decodes RFC 2047 encoded words).
        let values = message.header_as(header_name, HeaderForm::Text);
        if values.len() > HEADER_MAX_PER_NAME {
            truncated = true;
        }
        let mut ordinal: u32 = 0;
        for value in values.into_iter().take(HEADER_MAX_PER_NAME) {
            if out.len() >= HEADER_MAX_PER_MESSAGE {
                truncated = true;
                break;
            }
            let (value, cut) = clean_value(value.as_text().unwrap_or_default());
            if is_credential_like_value(&value) {
                // Never stored, whatever the header is called; something was
                // dropped, so the capture is partial.
                truncated = true;
                continue;
            }
            truncated |= cut;
            out.push(StoredHeader {
                name: name.clone(),
                ordinal,
                value,
            });
            ordinal += 1;
        }
    }
    (out, truncated)
}

/// Collapse every whitespace run (including stray CR/LF/tab left by folding)
/// to one space, trim, and keep at most HEADER_VALUE_MAX_BYTES. NULs and other
/// control characters are dropped: they have no business in a stored header
/// value (and this is what keeps [`HEADER_VALUE_CUT_MARKER`] unambiguous).
///
/// Returns `(value, cut)`. When the value does not fit, it is cut on a char
/// boundary so that the prefix plus the cut marker fit, the marker is
/// appended, and `cut` is true: an `Equals` can then never match the stored
/// stub, and the caller marks the capture partial.
fn clean_value(s: &str) -> (String, bool) {
    let marker_len = HEADER_VALUE_CUT_MARKER.len_utf8();
    let mut out = String::with_capacity(s.len().min(HEADER_VALUE_MAX_BYTES));
    // Byte length of `out` at the last point where the marker still fits.
    let mut fits_with_marker = 0;
    let mut pending_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if ch.is_control() {
            continue;
        }
        let add = ch.len_utf8() + usize::from(pending_space);
        if out.len() + add > HEADER_VALUE_MAX_BYTES {
            out.truncate(fits_with_marker);
            // A cut right after a space would leave a trailing space.
            let trimmed = out.trim_end().len();
            out.truncate(trimmed);
            out.push(HEADER_VALUE_CUT_MARKER);
            return (out, true);
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
        if out.len() + marker_len <= HEADER_VALUE_MAX_BYTES {
            fits_with_marker = out.len();
        }
    }
    (out, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'a>(hs: &'a [StoredHeader], name: &str) -> Vec<&'a str> {
        hs.iter()
            .filter(|h| h.name == name)
            .map(|h| h.value.as_str())
            .collect()
    }

    const MSG: &[u8] = b"Received: from a by b; Mon, 1 Jan 2026 00:00:00 +0000\r\n\
Received: from c by d\r\n\
From: Ann <ann@example.org>\r\n\
Subject: =?UTF-8?B?QnV0bGxldMOtIGRlIG5vdMOtY2llcw==?=\r\n\
List-Id: Dev list\r\n <dev.example.org>\r\n\
X-Mailer: Foo\r\n\
x-mailer: Bar\r\n\
Message-ID: <1@x>\r\n\
Authorization: Bearer abc\r\n\
X-Api-Token: s3cret\r\n\
Content-Type: text/plain\r\n\
\r\n\
body text\r\n";

    #[test]
    fn keeps_wanted_headers_and_drops_the_denylist() {
        let (hs, _) = extract_stored_headers(MSG);
        let names: Vec<&str> = hs.iter().map(|h| h.name.as_str()).collect();
        for banned in [
            "received",
            "message-id",
            "content-type",
            "authorization",
            "x-api-token",
        ] {
            assert!(!names.contains(&banned), "{banned} must not be stored");
        }
        // Built-in field names (from, subject, ...) are not Storable either.
        assert!(!names.contains(&"from") && !names.contains(&"subject"));
        assert!(names.contains(&"list-id") && names.contains(&"x-mailer"));
        // No credential value anywhere in the output.
        assert!(hs
            .iter()
            .all(|h| !h.value.contains("abc") && !h.value.contains("s3cret")));
    }

    #[test]
    fn folded_value_is_unfolded_and_repeats_keep_document_order() {
        let (hs, _) = extract_stored_headers(MSG);
        assert_eq!(get(&hs, "list-id"), ["Dev list <dev.example.org>"]);
        assert_eq!(get(&hs, "x-mailer"), ["Foo", "Bar"]);
        let ords: Vec<u32> = hs
            .iter()
            .filter(|h| h.name == "x-mailer")
            .map(|h| h.ordinal)
            .collect();
        assert_eq!(ords, [0, 1]);
    }

    #[test]
    fn rfc2047_value_is_decoded() {
        let raw = b"X-Title: =?UTF-8?B?QnV0bGxldMOtIGRlIG5vdMOtY2llcw==?=\r\nX-Q: =?ISO-8859-1?Q?caf=E9?=\r\n\r\n";
        let (hs, _) = extract_stored_headers(raw);
        assert_eq!(get(&hs, "x-title"), ["Butlletí de notícies"]);
        assert_eq!(get(&hs, "x-q"), ["café"]);
    }

    #[test]
    fn header_only_input_works_and_garbage_yields_nothing_or_no_panic() {
        let (hs, _) = extract_stored_headers(b"X-Foo: bar\r\nList-Id: <a.b>\r\n");
        assert_eq!(get(&hs, "x-foo"), ["bar"]);
        // Bad encoded words and binary junk must not panic.
        let _ = extract_stored_headers(b"X-A: =?UTF-8?B?!!!!?= =?bogus?Q?x?=\r\n\r\n");
        let _ = extract_stored_headers(&[0xff, 0xfe, 0x00, 0x3a, 0x0d, 0x0a, 0x80]);
        assert!(extract_stored_headers(b"").0.is_empty());
        assert!(extract_stored_headers(&[0u8; 64]).0.is_empty());
    }

    #[test]
    fn caps_per_name_per_message_and_value_size() {
        let mut raw = String::new();
        for i in 0..20 {
            raw.push_str(&format!("X-Rep: v{i}\r\n"));
        }
        for i in 0..100 {
            raw.push_str(&format!("X-N{i}: v\r\n"));
        }
        raw.push_str("\r\n");
        let (hs, _) = extract_stored_headers(raw.as_bytes());
        assert_eq!(get(&hs, "x-rep").len(), HEADER_MAX_PER_NAME);
        assert_eq!(get(&hs, "x-rep")[0], "v0");
        assert_eq!(hs.len(), HEADER_MAX_PER_MESSAGE);
    }

    #[test]
    fn one_megabyte_header_is_truncated_on_a_char_boundary() {
        let mut raw = b"X-Big: ".to_vec();
        raw.extend("é".repeat(600_000).as_bytes());
        raw.extend(b"\r\n\r\n");
        let (hs, truncated) = extract_stored_headers(&raw);
        assert!(truncated, "a 1.2 MB header block exceeds the input cap");
        let v = &get(&hs, "x-big");
        assert!(v.len() <= 1);
        if let Some(v) = v.first() {
            assert!(v.len() <= HEADER_VALUE_MAX_BYTES);
            // The line is cut by the input cap mid-value (mail-parser may then
            // keep nothing of it); whatever is kept is é's plus the marker.
            assert!(v
                .trim_end_matches(HEADER_VALUE_CUT_MARKER)
                .chars()
                .all(|c| c == 'é'));
        }
    }

    /// MUST 1 (review): the input cap applies to the header block only. A
    /// 2 MiB attachment after a normal header block is a COMPLETE capture.
    #[test]
    fn large_body_does_not_make_the_capture_partial() {
        let mut raw = b"From: a@example.org\r\nList-Id: <dev.example.org>\r\n\
X-Mailer: Foo\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
--b\r\nContent-Type: text/plain\r\n\r\nhi\r\n\
--b\r\nContent-Type: application/octet-stream\r\n\
Content-Transfer-Encoding: base64\r\n\r\n"
            .to_vec();
        let line = [b'A'; 76];
        while raw.len() < 2 * INPUT_CAP_BYTES + 10 {
            raw.extend_from_slice(&line);
            raw.extend_from_slice(b"\r\n");
        }
        raw.extend_from_slice(b"--b--\r\n");
        assert!(raw.len() > 2 * INPUT_CAP_BYTES);
        let (hs, truncated) = extract_stored_headers(&raw);
        assert!(!truncated, "a large body must not make the capture partial");
        assert_eq!(get(&hs, "list-id"), ["<dev.example.org>"]);
        assert_eq!(get(&hs, "x-mailer"), ["Foo"]);
        // LF-only line endings are found the same way.
        let mut lf = b"X-A: 1\nX-B: 2\n\n".to_vec();
        lf.extend(vec![b'z'; INPUT_CAP_BYTES + 5]);
        let (hs, truncated) = extract_stored_headers(&lf);
        assert!(!truncated);
        assert_eq!((get(&hs, "x-a"), get(&hs, "x-b")), (vec!["1"], vec!["2"]));
    }

    /// MUST 2 (review): a cut value reports truncation and carries the cut
    /// marker, so an exact comparison on the stub can never succeed.
    #[test]
    fn cut_value_is_flagged_and_marked() {
        let long = format!("X-Long: {}\r\nX-Short: ok\r\n\r\n", "a".repeat(2000));
        let (hs, truncated) = extract_stored_headers(long.as_bytes());
        assert!(truncated, "a cut value makes the capture partial");
        let v = get(&hs, "x-long")[0];
        assert!(v.len() <= HEADER_VALUE_MAX_BYTES);
        assert!(v.ends_with(HEADER_VALUE_CUT_MARKER));
        assert!(v
            .trim_end_matches(HEADER_VALUE_CUT_MARKER)
            .chars()
            .all(|c| c == 'a'));
        assert_eq!(get(&hs, "x-short"), ["ok"]);
        // Exactly at the limit: stored whole, not cut, not partial.
        let exact = format!("X-Exact: {}\r\n\r\n", "b".repeat(HEADER_VALUE_MAX_BYTES));
        let (hs, truncated) = extract_stored_headers(exact.as_bytes());
        assert!(!truncated);
        assert_eq!(get(&hs, "x-exact")[0].len(), HEADER_VALUE_MAX_BYTES);
        // Over the limit by one byte: the stub leaves room for the marker.
        let (v, cut) = clean_value(&format!("{} {}", "c".repeat(1022), "dd"));
        assert!(cut);
        assert_eq!(v, format!("{}{HEADER_VALUE_CUT_MARKER}", "c".repeat(1022)));
        assert_eq!(v.len(), HEADER_VALUE_MAX_BYTES);
        // Control characters (the marker itself included) never survive.
        assert_eq!(clean_value("a\u{80}b\u{0}c"), ("abc".to_string(), false));
    }

    /// NIT (review): a non-continuation line without a colon is skipped by
    /// mail-parser; the capture must then be partial, not silently complete.
    #[test]
    fn colonless_header_line_marks_the_capture_partial() {
        let raw = b"X-A: 1\r\nthis line is not a header\r\nList-Id: <a.b>\r\n\r\nbody\r\n";
        let (hs, truncated) = extract_stored_headers(raw);
        assert!(truncated);
        assert_eq!(get(&hs, "x-a"), ["1"]);
        // A folded continuation line is not suspect.
        let ok = b"X-A: 1\r\n  more of it\r\n\r\nbody:\r\nno colon here\r\n";
        let (hs, truncated) = extract_stored_headers(ok);
        assert!(!truncated);
        assert_eq!(get(&hs, "x-a"), ["1 more of it"]);
    }

    /// SECURITY (re-review): a whitespace-only line is a folded continuation
    /// for mail-parser, not the end of the headers. Ending the block there let
    /// a sender hide every header below it (e.g. List-Id appended by list
    /// software) while the capture still read as complete, so "List-Id
    /// absent" matched conclusively. The block must continue, List-Id must
    /// still be captured, and the capture is marked partial.
    #[test]
    fn whitespace_only_line_does_not_end_the_header_block() {
        let raw = b"X-A: 1\r\n \r\nList-Id: x\r\n\r\nbody\r\n";
        let (hs, truncated) = extract_stored_headers(raw);
        assert!(
            truncated,
            "a whitespace-only line makes the capture partial"
        );
        assert_eq!(
            get(&hs, "list-id"),
            ["x"],
            "headers after it are still captured"
        );
        let tab = b"X-A: 1\r\n\t\r\nList-Id: y\r\n\r\n";
        let (hs, truncated) = extract_stored_headers(tab);
        assert!(truncated);
        assert_eq!(get(&hs, "list-id"), ["y"]);
        // A truly empty line still ends the block, and a body line is not read.
        let (hs, truncated) = extract_stored_headers(b"X-A: 1\r\n\r\nList-Id: z\r\n");
        assert!(!truncated);
        assert!(get(&hs, "list-id").is_empty());
    }

    /// SHOULD 5 (review): `Bearer ...` / `Basic ...` values are never stored,
    /// whatever the header name, and the capture is partial.
    #[test]
    fn credential_like_values_are_dropped() {
        let raw = b"X-Forwarded-For-Thing: Bearer abc.def\r\nX-Note: basic dXNlcg==\r\n\
X-Note: hello\r\n\r\n";
        let (hs, truncated) = extract_stored_headers(raw);
        assert!(truncated);
        assert!(get(&hs, "x-forwarded-for-thing").is_empty());
        assert_eq!(get(&hs, "x-note"), ["hello"]);
        assert_eq!(hs.iter().find(|h| h.name == "x-note").unwrap().ordinal, 0);
        assert!(hs
            .iter()
            .all(|h| !h.value.contains("abc") && !h.value.contains("dXNlcg")));
    }

    #[test]
    fn many_denylisted_headers_are_cheap_and_dropped() {
        let mut raw = String::new();
        for i in 0..5000 {
            raw.push_str(&format!("Received: hop {i}\r\n"));
        }
        raw.push_str("X-Keep: yes\r\n\r\n");
        let (hs, _) = extract_stored_headers(raw.as_bytes());
        assert_eq!(hs.len(), 1);
        assert_eq!(hs[0].name, "x-keep");
    }

    #[test]
    fn truncation_is_reported_at_every_cap_and_on_parse_failure() {
        // Clean message: complete.
        assert!(!extract_stored_headers(b"X-A: 1\r\nList-Id: <a.b>\r\n\r\n").1);
        // Per-name cap.
        let mut raw = String::new();
        for i in 0..=HEADER_MAX_PER_NAME {
            raw.push_str(&format!("X-Rep: v{i}\r\n"));
        }
        assert!(extract_stored_headers(raw.as_bytes()).1, "per-name cap");
        // Exactly at the per-name cap is still complete.
        let mut raw = String::new();
        for i in 0..HEADER_MAX_PER_NAME {
            raw.push_str(&format!("X-Rep: v{i}\r\n"));
        }
        assert!(!extract_stored_headers(raw.as_bytes()).1);
        // Per-message cap.
        let mut raw = String::new();
        for i in 0..=HEADER_MAX_PER_MESSAGE {
            raw.push_str(&format!("X-N{i}: v\r\n"));
        }
        assert!(extract_stored_headers(raw.as_bytes()).1, "per-message cap");
        // Input cap: a header block (no blank line) running past the cap.
        let mut raw = b"X-A: 1\r\nX-B: ".to_vec();
        raw.extend(vec![b'x'; INPUT_CAP_BYTES + 1]);
        assert!(extract_stored_headers(&raw).1, "input cap");
        // Parse failure / empty input is never a complete capture.
        assert!(extract_stored_headers(b"").1);
        assert!(extract_stored_headers(&[0u8; 64]).1);
    }

    #[test]
    fn names_are_case_insensitive() {
        let (hs, _) = extract_stored_headers(b"LIST-ID: <a.b>\r\nList-iD: <c.d>\r\n\r\n");
        assert_eq!(get(&hs, "list-id"), ["<a.b>", "<c.d>"]);
    }
}
