// Castellum - RFC 2047 encoded-word decoding for IMAP ENVELOPE text.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

//! RFC 2047 "encoded word" decoding for the text fields of an IMAP ENVELOPE
//!.
//!
//! A standards-following server returns the Subject and the address display
//! names of an ENVELOPE as they sit in the header, so anything non-ASCII
//! arrives as `=?charset?Q?...?=` or `=?charset?B?...?=`. Gmail returns raw
//! UTF-8 instead. [`decode_encoded_words`] turns the first shape into text
//! and leaves the second exactly as it is.
//!
//! Usage:
//!
//! ```
//! use castellum_provider_imap::encoded_words::decode_encoded_words;
//! assert_eq!(decode_encoded_words("=?UTF-8?Q?Reuni=C3=B3?= de dem=C3=A0"), "Reunió de dem=C3=A0");
//! assert_eq!(decode_encoded_words("Café — raw"), "Café — raw");
//! ```
//!
//! Rules, in the order they matter:
//!
//! - Text with no well-formed encoded word comes back byte for byte.
//! - `Q` and `B` encodings, either letter case. Charset labels are matched
//!   case-insensitively and an RFC 2231 language suffix (`utf-8*ca`) is
//!   ignored. UTF-8 is decoded strictly here; every other charset goes
//!   through mail-parser's tables (ISO-8859-x, windows-125x, KOI8, UTF-16,
//!   UTF-7, ...), the same ones a hydrated body's headers are decoded with.
//! - White space between two adjacent encoded words is dropped (RFC 2047
//!   section 6.2); white space next to plain text is kept.
//! - Adjacent words in the same charset are joined before the charset is
//!   applied, so a multi-byte character split across two words (which the
//!   RFC forbids and real senders do anyway) still decodes.
//! - A word that is malformed, names a charset we cannot decode, or whose
//!   bytes are not valid in its charset is left as the original text. The
//!   multi-byte CJK charsets (Shift_JIS, ISO-2022-JP, GBK, Big5, EUC-KR, ...)
//!   decode because this crate enables mail-parser's `full_encoding` feature
//!   (encoding_rs); invalid bytes still leave the word as the original text
//!   rather than mojibake.
//! - Control characters produced by decoding (CR, LF, NUL, ...) become
//!   spaces: this text feeds single-line list rows and notifications.
//! - It never panics, whatever the input: everything is bounds-checked
//!   slicing at ASCII positions, and the work is linear in the input.
//!
//! Callers must not run already-decoded text through this a second time:
//! decoded text may legitimately contain something shaped like an encoded
//! word. The ENVELOPE path decodes once, in `session`; a hydrated body's
//! headers are decoded by mail-parser and never come through here.

use base64::alphabet;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::engine::DecodePaddingMode;
use base64::Engine;
use mail_parser::decoders::charsets::map::charset_decoder;

/// Decode every RFC 2047 encoded word in `text`. See the module docs for the
/// exact rules; text without an encoded word is returned unchanged.
pub fn decode_encoded_words(text: &str) -> String {
    // The Gmail shape, and almost every ASCII subject: nothing to do.
    if !text.contains("=?") {
        return text.to_string();
    }
    let pieces = split_pieces(text);
    let decoded = decode_words(&pieces);

    let mut out = String::with_capacity(text.len());
    for (idx, piece) in pieces.iter().enumerate() {
        match piece {
            Piece::Text(plain) => {
                // RFC 2047 section 6.2: white space that only separates two
                // encoded words is not part of the text. Both neighbours
                // must actually have decoded; next to a word we left alone
                // the white space is part of the original and stays.
                let separates_two_words = idx > 0
                    && decoded[idx - 1].is_some()
                    && decoded.get(idx + 1).is_some_and(Option::is_some)
                    && is_linear_white_space(plain);
                if !separates_two_words {
                    out.push_str(plain);
                }
            }
            Piece::Word(word) => match &decoded[idx] {
                Some(value) => {
                    out.extend(value.chars().map(|c| if c.is_control() { ' ' } else { c }))
                }
                None => out.push_str(word.raw),
            },
        }
    }
    out
}

/// [`decode_encoded_words`], but `None` when decoding changes nothing — the
/// text holds no encoded word, or only ones that cannot be decoded. For
/// callers that must not touch a value that needs no repair.
pub fn decode_encoded_words_if_any(text: &str) -> Option<String> {
    if !text.contains("=?") {
        return None;
    }
    let decoded = decode_encoded_words(text);
    (decoded != text).then_some(decoded)
}

/// One syntactically complete encoded word, `=?charset[*lang]?enc?payload?=`.
struct Word<'a> {
    /// The word exactly as it appeared, for when it cannot be decoded.
    raw: &'a str,
    /// Charset label with any RFC 2231 `*language` suffix removed.
    charset: &'a str,
    /// The payload after Q or B transfer decoding; `None` if it is malformed.
    bytes: Option<Vec<u8>>,
}

enum Piece<'a> {
    Text(&'a str),
    Word(Word<'a>),
}

fn is_linear_white_space(text: &str) -> bool {
    text.bytes()
        .all(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
}

/// Cut `text` into plain runs and encoded words. Every index used to slice
/// is the position of an ASCII byte, so it is always a char boundary.
fn split_pieces(text: &str) -> Vec<Piece<'_>> {
    let mut pieces = Vec::new();
    let mut plain_start = 0;
    let mut cursor = 0;
    while let Some(offset) = text.get(cursor..).and_then(|rest| rest.find("=?")) {
        let start = cursor + offset;
        match text.get(start..).and_then(parse_word) {
            Some(word) => {
                if start > plain_start {
                    pieces.push(Piece::Text(&text[plain_start..start]));
                }
                cursor = start + word.raw.len();
                plain_start = cursor;
                pieces.push(Piece::Word(word));
            }
            // Not a word: step over this opener and keep looking.
            None => cursor = start + 2,
        }
    }
    if plain_start < text.len() {
        pieces.push(Piece::Text(&text[plain_start..]));
    }
    pieces
}

/// Parse the encoded word `s` starts with (`s` begins with `=?`). `None`
/// when the shape is wrong: no charset, an encoding other than Q or B, or no
/// `?=` terminator. Each scan stops at the next `?`, so a run of openers
/// cannot make the caller quadratic.
fn parse_word(s: &str) -> Option<Word<'_>> {
    let bytes = s.as_bytes();
    let label_end = 2 + bytes.get(2..)?.iter().position(|&b| b == b'?')?;
    let label = s.get(2..label_end)?;
    if !label.bytes().all(|b| b.is_ascii_graphic()) {
        return None;
    }
    let charset = label.split('*').next().unwrap_or(label);
    if charset.is_empty() {
        return None;
    }
    let encoding = *bytes.get(label_end + 1)?;
    if *bytes.get(label_end + 2)? != b'?' {
        return None;
    }
    let payload_start = label_end + 3;
    let payload_end = payload_start
        + bytes
            .get(payload_start..)?
            .iter()
            .position(|&b| b == b'?')?;
    if *bytes.get(payload_end + 1)? != b'=' {
        return None;
    }
    let payload = bytes.get(payload_start..payload_end)?;
    let decoded = match encoding {
        b'q' | b'Q' => decode_q(payload),
        b'b' | b'B' => decode_b(payload),
        _ => return None,
    };
    Some(Word {
        raw: s.get(..payload_end + 2)?,
        charset,
        bytes: decoded,
    })
}

/// RFC 2047 "Q": `_` is a space, `=XX` is a byte, everything else is itself.
/// A raw space is tolerated (senders that never read the RFC produce it);
/// a broken `=` escape or a control character makes the word malformed.
fn decode_q(payload: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(payload.len());
    let mut rest = payload.iter().copied();
    while let Some(byte) = rest.next() {
        match byte {
            b'_' => out.push(b' '),
            b'=' => {
                let high = hex_value(rest.next()?)?;
                let low = hex_value(rest.next()?)?;
                out.push(high << 4 | low);
            }
            0..=0x1F | 0x7F => return None,
            other => out.push(other),
        }
    }
    Some(out)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// RFC 2047 "B": standard base64. Missing padding is tolerated (common in
/// the wild); anything outside the alphabet makes the word malformed.
fn decode_b(payload: &[u8]) -> Option<Vec<u8>> {
    const LENIENT_PADDING: GeneralPurpose = GeneralPurpose::new(
        &alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_padding_mode(DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    );
    LENIENT_PADDING.decode(payload).ok()
}

/// Bytes in `charset` to text, or `None` if we cannot do it faithfully.
fn decode_charset(charset: &str, bytes: &[u8]) -> Option<String> {
    if charset.eq_ignore_ascii_case("utf-8") || charset.eq_ignore_ascii_case("utf8") {
        return String::from_utf8(bytes.to_vec()).ok();
    }
    let decoder = charset_decoder(charset.as_bytes())?;
    let text = decoder(bytes);
    // A replacement character means the bytes were not valid in that
    // charset, or that it is one of the multi-byte charsets mail-parser
    // only stubs out without `full_encoding`. Either way: not faithful.
    (!text.contains('\u{FFFD}')).then_some(text)
}

/// For every piece, the decoded text if it is an encoded word that decoded.
/// A run of same-charset words separated only by white space is decoded as
/// one byte string (its text lands on the first word, the rest are empty),
/// so a character split across two words survives; if the joined run does
/// not decode, each word is tried on its own.
fn decode_words(pieces: &[Piece<'_>]) -> Vec<Option<String>> {
    let mut decoded: Vec<Option<String>> = vec![None; pieces.len()];
    let mut idx = 0;
    while idx < pieces.len() {
        let Piece::Word(first) = &pieces[idx] else {
            idx += 1;
            continue;
        };
        let Some(first_bytes) = &first.bytes else {
            idx += 1;
            continue;
        };
        let mut run = vec![idx];
        let mut joined = first_bytes.clone();
        let mut next = idx + 1;
        loop {
            let candidate = match pieces.get(next) {
                Some(Piece::Text(gap)) if is_linear_white_space(gap) => next + 1,
                Some(Piece::Word(_)) => next,
                Some(Piece::Text(_)) | None => break,
            };
            match pieces.get(candidate) {
                Some(Piece::Word(word)) if word.charset.eq_ignore_ascii_case(first.charset) => {
                    let Some(bytes) = &word.bytes else { break };
                    joined.extend_from_slice(bytes);
                    run.push(candidate);
                    next = candidate + 1;
                }
                _ => break,
            }
        }
        match decode_charset(first.charset, &joined) {
            Some(text) => {
                for &member in &run[1..] {
                    decoded[member] = Some(String::new());
                }
                decoded[idx] = Some(text);
            }
            None if run.len() > 1 => {
                for &member in &run {
                    if let Piece::Word(word) = &pieces[member] {
                        decoded[member] = word
                            .bytes
                            .as_deref()
                            .and_then(|bytes| decode_charset(word.charset, bytes));
                    }
                }
            }
            None => {}
        }
        idx = next;
    }
    decoded
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (input, expected) — one row per rule in the stage brief.
    const TABLE: &[(&str, &str, &str)] = &[
        // --- Q encoding, the four charsets Catalan and Spanish mail uses
        (
            "Q, UTF-8 (GreenMail persona subject)",
            "=?UTF-8?Q?Solutions_Architect_role_=E2=80=94_Cambridge=2C_hybrid?=",
            "Solutions Architect role — Cambridge, hybrid",
        ),
        (
            "Q, UTF-8 (second persona subject, en dash)",
            "=?UTF-8?Q?Scheduled_maintenance=3A_Sat_13_Sep_02=3A00=E2=80=9303=3A00_UTC?=",
            "Scheduled maintenance: Sat 13 Sep 02:00–03:00 UTC",
        ),
        (
            "Q, ISO-8859-1",
            "=?ISO-8859-1?Q?Reuni=F3_de_dem=E0?=",
            "Reunió de demà",
        ),
        (
            "Q, ISO-8859-15 (euro sign at 0xA4)",
            "=?ISO-8859-15?Q?Factura_de_30_=A4?=",
            "Factura de 30 €",
        ),
        (
            "Q, windows-1252 (curly quotes and euro in 0x80-0x9F)",
            "=?windows-1252?Q?=93A=F1o_nuevo=94_=80?=",
            "\u{201C}Año nuevo\u{201D} €",
        ),
        // --- B encoding, same four charsets
        (
            "B, UTF-8",
            "=?UTF-8?B?Q2Fmw6kg4oCUIHLDqXVuacOz?=",
            "Café — réunió",
        ),
        (
            "B, ISO-8859-1",
            "=?ISO-8859-1?B?QmFy52EgYWxlZ3Jl?=",
            "Barça alegre",
        ),
        (
            "B, ISO-8859-15",
            "=?ISO-8859-15?B?TnVyaWEgUHVpZyAopCk=?=",
            "Nuria Puig (€)",
        ),
        (
            "B, windows-1252",
            "=?windows-1252?B?U2XxYWwgk29rlA==?=",
            "Señal \u{201C}ok\u{201D}",
        ),
        (
            "B, missing padding is tolerated",
            "=?UTF-8?B?Q2Fmw6k?=",
            "Café",
        ),
        // --- letter case
        ("lower-case charset and q", "=?utf-8?q?caf=c3=a9?=", "café"),
        ("lower-case b", "=?iso-8859-1?b?QmFy52E=?=", "Barça"),
        ("mixed-case charset", "=?Iso-8859-15?Q?=A4?=", "€"),
        // --- RFC 2231 language suffix
        ("language suffix, Q", "=?utf-8*ca?Q?Mart=C3=AD?=", "Martí"),
        ("language suffix, B", "=?ISO-8859-1*es?B?QfFv?=", "Año"),
        // --- several encoded words in a row
        (
            "adjacent words: the space between them is dropped",
            "=?ISO-8859-1?Q?Reuni=F3?= =?ISO-8859-1?Q?_de_dem=E0?=",
            "Reunió de demà",
        ),
        (
            "adjacent words: folding white space (CRLF + tab) is dropped",
            "=?UTF-8?Q?a?=\r\n\t=?UTF-8?Q?b?=  =?UTF-8?Q?c?=",
            "abc",
        ),
        (
            "adjacent words with no space at all",
            "=?UTF-8?Q?a?==?UTF-8?Q?b?=",
            "ab",
        ),
        (
            "adjacent words in different charsets",
            "=?ISO-8859-1?Q?a=F1o?= =?UTF-8?Q?_=E2=82=AC?=",
            "año €",
        ),
        (
            "a multi-byte character split across two words",
            "=?UTF-8?B?Q2Fmww==?= =?UTF-8?B?qQ==?=",
            "Café",
        ),
        // --- an encoded word next to plain text
        (
            "plain text before and after keeps its white space",
            "Re:  =?UTF-8?Q?Reuni=C3=B3?=  de la junta",
            "Re:  Reunió  de la junta",
        ),
        (
            "plain text between two words keeps the spaces around it",
            "=?UTF-8?Q?caf=C3=A9?= i =?UTF-8?Q?t=C3=A8?=",
            "café i tè",
        ),
        (
            "word glued to plain text",
            "Fwd:=?UTF-8?Q?caf=C3=A9?=!",
            "Fwd:café!",
        ),
        (
            "underscore is a space, =5F an underscore",
            "=?UTF-8?Q?a_b=5Fc?=",
            "a b_c",
        ),
        (
            "a raw space inside a Q word is tolerated",
            "=?UTF-8?Q?hola m=C3=B3n?=",
            "hola món",
        ),
        ("empty payload", "a=?UTF-8?Q??=b", "ab"),
        // --- raw UTF-8 with no encoded word (Gmail) stays exactly as it is
        (
            "raw UTF-8",
            "Café réunió — \"quoted\" & <angled>",
            "Café réunió — \"quoted\" & <angled>",
        ),
        (
            "raw UTF-8 keeps odd spacing",
            "  dues   paraules\t",
            "  dues   paraules\t",
        ),
        ("plain ASCII", "Quarterly report", "Quarterly report"),
        ("empty", "", ""),
        // --- malformed or unknown-charset words are left as the original text
        (
            "unknown charset",
            "=?x-klingon?Q?nuqneH?=",
            "=?x-klingon?Q?nuqneH?=",
        ),
        (
            "Shift_JIS decodes now that full_encoding is on",
            "=?Shift_JIS?B?g2WDWINn?=",
            "\u{30c6}\u{30b9}\u{30c8}",
        ),
        (
            "ISO-2022-JP decodes now that full_encoding is on",
            "=?ISO-2022-JP?B?GyRCRnxLXDhsGyhC?=",
            "\u{65e5}\u{672c}\u{8a9e}",
        ),
        (
            "unknown encoding letter",
            "=?UTF-8?X?abc?=",
            "=?UTF-8?X?abc?=",
        ),
        (
            "truncated Q escape",
            "=?UTF-8?Q?caf=C3=A?=",
            "=?UTF-8?Q?caf=C3=A?=",
        ),
        (
            "non-hex Q escape",
            "=?UTF-8?Q?caf=ZZ?=",
            "=?UTF-8?Q?caf=ZZ?=",
        ),
        ("invalid base64", "=?UTF-8?B?@@@@?=", "=?UTF-8?B?@@@@?="),
        (
            "invalid UTF-8 bytes",
            "=?UTF-8?Q?=C3=28?=",
            "=?UTF-8?Q?=C3=28?=",
        ),
        ("no terminator", "=?UTF-8?Q?abc", "=?UTF-8?Q?abc"),
        ("missing encoding field", "=?UTF-8?abc?=", "=?UTF-8?abc?="),
        ("empty charset", "=??Q?abc?=", "=??Q?abc?="),
        (
            "empty charset before a language",
            "=?*ca?Q?abc?=",
            "=?*ca?Q?abc?=",
        ),
        (
            "stray question mark in the payload",
            "=?UTF-8?Q?a?b?=",
            "=?UTF-8?Q?a?b?=",
        ),
        ("just the opener", "=?", "=?"),
        ("opener and closer only", "=??=", "=??="),
        ("not a word at all", "a =? b ?= c", "a =? b ?= c"),
        (
            "a bad word keeps the white space around it; the good one still decodes",
            "=?UTF-8?Q?bo?= =?x-klingon?Q?nuqneH?= =?UTF-8?Q?d=C3=ADa?=",
            "bo =?x-klingon?Q?nuqneH?= día",
        ),
        // --- decoded control characters become spaces
        ("decoded CR LF NUL", "=?UTF-8?Q?a=0D=0Ab=00c?=", "a  b c"),
    ];

    #[test]
    fn table() {
        for (case, input, expected) in TABLE {
            assert_eq!(
                decode_encoded_words(input),
                *expected,
                "case: {case} (input {input:?})"
            );
        }
    }

    #[test]
    fn if_any_reports_only_real_changes() {
        assert_eq!(
            decode_encoded_words_if_any("=?UTF-8?Q?caf=C3=A9?=").as_deref(),
            Some("café")
        );
        // No encoded word, an undecodable one, and raw UTF-8: nothing to do.
        assert_eq!(decode_encoded_words_if_any("Quarterly report"), None);
        assert_eq!(decode_encoded_words_if_any("=?x-klingon?Q?nuqneH?="), None);
        assert_eq!(decode_encoded_words_if_any("Café — =? raw"), None);
    }

    /// Deterministic xorshift, so a failure reproduces.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// Fuzz-style: strings built from the fragments an encoded word is made
    /// of, in every wrong order, must never panic — and text with no `=?`
    /// in it must always come back unchanged.
    #[test]
    fn hostile_input_never_panics() {
        const FRAGMENTS: &[&str] = &[
            "=?",
            "?=",
            "?",
            "=",
            "*",
            "_",
            " ",
            "\t",
            "\r\n",
            "Q",
            "q",
            "B",
            "b",
            "UTF-8",
            "utf-8*ca",
            "ISO-8859-1",
            "iso-8859-15",
            "windows-1252",
            "utf-16",
            "utf-7",
            "Shift_JIS",
            "iso-2022-jp",
            "x",
            "=C3",
            "=A9",
            "=E2=80",
            "=0",
            "=ZZ",
            "w6k",
            "====",
            "+AGE-",
            "é",
            "—",
            "€",
            "\u{0}",
            "\u{FFFD}",
            "🙂",
            "a",
            "?Q?",
            "?B?",
            "=?UTF-8?Q?",
            "=?UTF-8?B?",
            "=?ISO-8859-1?Q?",
        ];
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..20_000 {
            let n = (rng.next() % 12) as usize;
            let mut input = String::new();
            for _ in 0..n {
                input.push_str(FRAGMENTS[(rng.next() % FRAGMENTS.len() as u64) as usize]);
            }
            let out = decode_encoded_words(&input);
            if !input.contains("=?") {
                assert_eq!(out, input, "no opener, so nothing may change");
            }
            assert!(
                !out.chars().any(|c| c.is_control()) || input.chars().any(|c| c.is_control()),
                "decoding must not introduce control characters: {input:?} -> {out:?}"
            );
            let _ = decode_encoded_words_if_any(&input);
        }
    }

    /// Pathological sizes and shapes: a megabyte of openers, a very long
    /// charset label, a huge payload with no terminator.
    #[test]
    fn hostile_sizes_stay_linear_and_never_panic() {
        let openers = "=?".repeat(500_000);
        assert_eq!(decode_encoded_words(&openers), openers);

        let long_charset = format!("=?{}?Q?abc?=", "x".repeat(100_000));
        assert_eq!(decode_encoded_words(&long_charset), long_charset);

        let unterminated = format!("=?UTF-8?Q?{}", "a".repeat(1_000_000));
        assert_eq!(decode_encoded_words(&unterminated), unterminated);

        let many = "=?UTF-8?Q?=C3=A9?= ".repeat(50_000);
        let decoded = decode_encoded_words(&many);
        assert_eq!(decoded.chars().filter(|c| *c == 'é').count(), 50_000);

        let nested = "=?UTF-8?Q?=?UTF-8?Q?=?UTF-8?Q?a?=?=?=";
        let _ = decode_encoded_words(nested);
    }
}
