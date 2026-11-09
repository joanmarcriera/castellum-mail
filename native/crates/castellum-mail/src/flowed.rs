// Castellum - RFC 3676 `format=flowed` decoding (MM-17, slice 1).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// A `text/plain; format=flowed` body is "soft-wrapped" at about 72 columns by
// the sender: a line that ENDS IN A SPACE continues on the next line. Shown
// raw it has ragged 72-column lines; decoded, each paragraph is one logical
// line that the reading pane wraps to its own width. Fixed-format mail never
// reaches this module.
//
// `decode` runs once at parse time, so the stored `text_body`, the snippet,
// search, the reply quote and AI all see logical paragraphs. Quoting is kept
// in the usual `> text` shape (one space after the markers) so everything that
// already handles quoted text keeps working.
//
// Total over any input: no panics, no byte-offset slicing.

/// Decode RFC 3676 flowed text. `delsp` is the part's `DelSp=yes` parameter:
/// the single space that marks a soft break is then deleted when the lines are
/// joined (for scripts written without spaces); otherwise it is kept as the
/// word separator.
///
/// Rules (RFC 3676 section 4.5):
/// * quote depth = the number of leading `>`; one space after the markers, or
///   one leading space on an unquoted line, is "space-stuffing" and removed;
/// * a line ending in a space is flowed and joins the next line, but ONLY when
///   the next line has the same quote depth (a depth change ends the paragraph);
/// * the signature separator `-- ` is never flowed and never joined onto.
pub fn decode(text: &str, delsp: bool) -> String {
    let mut out: Vec<String> = Vec::new();
    // The paragraph being assembled: (quote depth, text so far, still flowing).
    let mut open: Option<(usize, String)> = None;

    fn emit(out: &mut Vec<String>, depth: usize, body: &str) {
        let mut line = ">".repeat(depth);
        if depth > 0 && !body.is_empty() {
            line.push(' ');
        }
        line.push_str(body);
        out.push(line);
    }

    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let depth = raw.chars().take_while(|&c| c == '>').count();
        // `>` is one byte, so `depth` is a valid byte offset.
        let rest = &raw[depth..];
        let rest = rest.strip_prefix(' ').unwrap_or(rest);

        // A paragraph left open at a different depth closes first.
        if let Some((open_depth, _)) = &open {
            if *open_depth != depth {
                if let Some((d, body)) = open.take() {
                    emit(&mut out, d, body.trim_end_matches(' '));
                }
            }
        }

        let is_signature_separator = rest == "-- ";
        // RFC 3676 section 4.3: the separator is never joined onto. A
        // paragraph still open (its previous line ended in a space) closes
        // first, so the separator stays a line of its own and the signature
        // below it can still be recognised.
        if is_signature_separator {
            if let Some((d, body)) = open.take() {
                emit(&mut out, d, body.trim_end_matches(' '));
            }
        }
        let flowed = rest.ends_with(' ') && !is_signature_separator;
        let piece = if flowed && delsp {
            &rest[..rest.len() - 1]
        } else {
            rest
        };

        match open.take() {
            Some((d, mut body)) => {
                // Same depth by the check above: continue the paragraph.
                body.push_str(piece);
                if flowed {
                    open = Some((d, body));
                } else {
                    emit(&mut out, d, &body);
                }
            }
            None => {
                if flowed {
                    open = Some((depth, piece.to_string()));
                } else {
                    emit(&mut out, depth, piece);
                }
            }
        }
    }
    // Input ended mid-paragraph: close it as a fixed line.
    if let Some((d, body)) = open {
        emit(&mut out, d, body.trim_end_matches(' '));
    }
    out.join("\n")
}

/// Soft-wrap column for [`encode`] (RFC 3676 section 4.2 recommends under 79).
/// The trailing space of a soft break counts toward it.
pub const WRAP_WIDTH: usize = 72;

/// Encode logical text as RFC 3676 `format=flowed` with `DelSp=no`: the inverse
/// of [`decode`] for text in the shape `decode` produces.
///
/// Per input line (`\r\n` or `\n`; output uses `\n`, the SMTP builder owns
/// the wire line endings):
/// * leading `>` markers are the quote depth; the single space after them is
///   a separator, and every wrapped continuation repeats the markers;
/// * trailing spaces are trimmed (they would read as a soft break), except on
///   the signature separator `-- `, which is never wrapped and never flowed;
/// * an unquoted line starting with a space, `>` or `From ` is space-stuffed;
/// * a line longer than `width` is broken after the last run of spaces that
///   fits, each segment but the last keeping one trailing space. A word longer
///   than `width` (a URL) is never split, so its line may exceed `width`.
///
/// Total over any input; counts `char`s, never slices inside a character.
pub fn encode(text: &str, width: usize) -> String {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let depth = raw.chars().take_while(|&c| c == '>').count();
        let after = &raw[depth..]; // `>` is one byte
        let body: &str = if depth > 0 {
            after.strip_prefix(' ').unwrap_or(after)
        } else {
            after
        };
        if body == "-- " {
            out.push(format!(
                "{}{}-- ",
                ">".repeat(depth),
                if depth > 0 { " " } else { "" }
            ));
            continue;
        }
        let body = body.trim_end_matches(' ');
        let quote = ">".repeat(depth);
        let chars: Vec<char> = body.chars().collect();
        let mut start = 0usize;
        loop {
            let rest = &chars[start..];
            // Prefix of this physical line: quote markers + separator, or the
            // stuffing space an unquoted line needs.
            let starts_with_from =
                rest.len() >= 5 && rest[..5].iter().collect::<String>() == "From ";
            let prefix = if depth > 0 {
                if rest.is_empty() {
                    quote.clone()
                } else {
                    format!("{quote} ")
                }
            } else if rest.first().is_some_and(|&c| c == ' ' || c == '>') || starts_with_from {
                " ".to_string()
            } else {
                String::new()
            };
            let room = width.saturating_sub(prefix.chars().count());
            if rest.len() <= room {
                out.push(format!("{prefix}{}", rest.iter().collect::<String>()));
                break;
            }
            // A break index `i`: chars[i] is the LAST space of a run, followed
            // by a word, with some word text before it. The segment
            // `rest[..=i]` then keeps its trailing space and the continuation
            // never begins with a space.
            let ok = |i: usize| {
                rest[i] == ' '
                    && i + 1 < rest.len()
                    && rest[i + 1] != ' '
                    && rest[..i].iter().any(|&c| c != ' ')
                    // A soft segment of exactly `-- ` would be read back as a
                    // signature separator (hard break, text stripped).
                    && rest[..=i] != ['-', '-', ' ']
            };
            let brk = (0..room.min(rest.len()))
                .rev()
                .find(|&i| ok(i))
                .or_else(|| {
                    // Nothing fits: take the first break after the long word.
                    (room..rest.len()).find(|&i| ok(i))
                });
            match brk {
                Some(i) => {
                    out.push(format!("{prefix}{}", rest[..=i].iter().collect::<String>()));
                    start += i + 1;
                }
                None => {
                    out.push(format!("{prefix}{}", rest.iter().collect::<String>()));
                    break;
                }
            }
        }
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::{decode, encode, WRAP_WIDTH};

    #[test]
    fn soft_wrapped_lines_join_into_one_paragraph() {
        let flowed = "This is a long paragraph that the sender wrapped \nat seventy-two columns \nand then ended.\n\nNext paragraph.";
        assert_eq!(
            decode(flowed, false),
            "This is a long paragraph that the sender wrapped at seventy-two columns and then ended.\n\nNext paragraph."
        );
    }

    #[test]
    fn hard_lines_stay_separate() {
        assert_eq!(decode("one\ntwo\nthree", false), "one\ntwo\nthree");
    }

    #[test]
    fn delsp_removes_the_soft_break_space() {
        assert_eq!(decode("日本語 \nテキスト", true), "日本語テキスト");
        assert_eq!(decode("日本語 \nテキスト", false), "日本語 テキスト");
    }

    #[test]
    fn quoted_flowed_text_joins_within_its_depth() {
        let flowed = "> quoted line one \n> continues here\n>> deeper \n>> still deeper\nreply";
        assert_eq!(
            decode(flowed, false),
            "> quoted line one continues here\n>> deeper still deeper\nreply"
        );
    }

    #[test]
    fn a_depth_change_ends_the_paragraph_even_after_a_trailing_space() {
        assert_eq!(decode("> quoted \nplain", false), "> quoted\nplain");
    }

    #[test]
    fn space_stuffing_is_removed_once() {
        assert_eq!(decode(" From the start", false), "From the start");
        assert_eq!(decode("> >not nested", false), "> >not nested");
        assert_eq!(decode("  two spaces", false), " two spaces");
    }

    #[test]
    fn the_signature_separator_is_never_joined() {
        let flowed = "Thanks \nBob\n-- \nBob Builder \nbuilds things";
        assert_eq!(
            decode(flowed, false),
            "Thanks Bob\n-- \nBob Builder builds things"
        );
    }

    #[test]
    fn an_open_paragraph_closes_before_the_signature_separator() {
        assert_eq!(decode("Hello \n-- \nAlice", false), "Hello\n-- \nAlice");
        assert_eq!(decode("Hello \n-- \nAlice", true), "Hello\n-- \nAlice");
        assert_eq!(decode("> Hi \n> -- \n> Al", false), "> Hi\n> -- \n> Al");
    }

    #[test]
    fn empty_quoted_lines_keep_their_markers() {
        assert_eq!(decode("> a\n>\n> b", false), "> a\n>\n> b");
    }

    #[test]
    fn crlf_input_is_normalised() {
        assert_eq!(decode("a \r\nb\r\n", false), "a b\n");
    }

    #[test]
    fn a_trailing_flowed_line_at_end_of_input_is_closed() {
        assert_eq!(decode("dangling ", false), "dangling");
    }

    #[test]
    fn never_panics_on_arbitrary_input() {
        for s in [
            "",
            ">",
            ">>>>",
            " ",
            "\n\n\n",
            "é ",
            "> é \n> ü",
            "\u{FEFF}> x ",
            "-- ",
            "\r",
        ] {
            let _ = decode(s, false);
            let _ = decode(s, true);
        }
        let big = "word ".repeat(200_000);
        assert!(decode(&big, false).starts_with("word word"));
    }

    // ---- encode (MM-17 S2) ----

    fn assert_round_trip(x: &str) {
        let wire = encode(x, WRAP_WIDTH);
        let expect: Vec<String> = x
            .split('\n')
            .map(|l| {
                let l = l.strip_suffix('\r').unwrap_or(l);
                if l == "-- " || l == "> -- " {
                    l.to_string()
                } else {
                    l.trim_end_matches(' ').to_string()
                }
            })
            .collect();
        assert_eq!(decode(&wire, false), expect.join("\n"), "wire: {wire:?}");
    }

    #[test]
    fn encode_wraps_with_trailing_space_and_round_trips() {
        let x = "Aquest és un paràgraf bastant llarg escrit en català que s'ha de tallar en diverses línies suaus sense perdre cap paraula pel camí.\nSegona línia dura.";
        let wire = encode(x, WRAP_WIDTH);
        let lines: Vec<&str> = wire.split('\n').collect();
        assert!(lines.len() >= 3);
        for l in &lines[..lines.len() - 2] {
            assert!(l.ends_with(' '), "soft break needs a trailing space: {l:?}");
            assert!(l.chars().count() <= WRAP_WIDTH, "{l:?}");
        }
        assert_round_trip(x);
    }

    #[test]
    fn encode_never_splits_a_long_word() {
        let url = format!("https://example.com/{}", "a".repeat(300));
        let x = format!("see {url} for details and more words after the link to wrap again later");
        let wire = encode(&x, WRAP_WIDTH);
        assert!(wire.contains(&url), "url split: {wire:?}");
        assert!(wire
            .lines()
            .all(|l| l.chars().count() <= WRAP_WIDTH || l.contains(&url)));
        assert_round_trip(&x);
        let lone = "x".repeat(200);
        assert_eq!(encode(&lone, WRAP_WIDTH), lone);
    }

    #[test]
    fn encode_stuffs_space_gt_and_from() {
        assert_eq!(encode(" lead", 76), "  lead");
        // A leading `>` is a quote marker by definition, never stuffed.
        assert_eq!(encode(">gt", 76), "> gt");
        assert_eq!(encode("From me", 76), " From me");
        for x in [" lead", "From me", "Fromage", "  two"] {
            assert_round_trip(x);
        }
        // A continuation that would start with `From ` or `>` is stuffed too.
        let x = format!("{} From here > there and on", "w".repeat(WRAP_WIDTH - 2));
        let wire = encode(&x, WRAP_WIDTH);
        assert!(wire.contains("\n From here"), "{wire:?}");
        assert_round_trip(&x);
    }

    #[test]
    fn encode_signature_separator_stays_hard() {
        let x = "Thanks\n-- \nBob Builder\nbuilds things";
        let wire = encode(x, WRAP_WIDTH);
        assert!(wire.contains("\n-- \n"), "{wire:?}");
        assert_eq!(decode(&wire, false), x);
        // Trailing spaces elsewhere are trimmed, not turned into soft breaks.
        assert_eq!(decode(&encode("hello  \nworld", 76), false), "hello\nworld");
    }

    #[test]
    fn encode_never_emits_a_soft_signature_separator() {
        // "-- <long url>" must not break after "-- ": that line would decode as
        // a signature separator.
        let url = "u".repeat(120);
        for x in [
            format!("-- {url}"),
            format!("> -- {url}"),
            format!("text {url} -- {url}"),
            format!("a -- -- {url}"),
        ] {
            let wire = encode(&x, WRAP_WIDTH);
            for l in wire.split('\n') {
                assert!(
                    l != "-- " && l != "> -- ",
                    "soft segment is a separator in {wire:?}"
                );
            }
            assert_round_trip(&x);
        }
    }

    #[test]
    fn encode_keeps_quote_depth_on_every_wrapped_line() {
        let words = "lorem ipsum dolor sit amet ".repeat(8);
        let x = format!("> {words}\n>> {words}\n> \nreply");
        let wire = encode(&x, WRAP_WIDTH);
        for l in wire.lines().filter(|l| l.starts_with('>')) {
            assert!(l.chars().count() <= WRAP_WIDTH, "{l:?}");
        }
        assert!(wire.lines().filter(|l| l.starts_with(">> ")).count() >= 2);
        assert_round_trip(&x);
    }

    #[test]
    fn encode_crlf_unicode_and_cjk() {
        assert_round_trip("línia un\r\nlínia dos amb accents àèéíòóú ç ñ\r\n");
        assert_round_trip(&"日本語のテキスト ".repeat(30));
        assert_round_trip(&"emoji 🎉 party time ".repeat(20));
        assert_eq!(encode("a\r\nb", 76), "a\nb");
    }

    #[test]
    fn encode_round_trips_generated_text() {
        // Property-style: a deterministic LCG builds lines of short words, long
        // words, quotes, stuffing triggers and multiple spaces; every width.
        let mut seed: u64 = 0x2807;
        let mut next = move |n: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        let vocab = [
            "a",
            "word",
            "ünï",
            ">",
            "From",
            "-",
            "日本",
            "x".repeat(90).leak() as &str,
            "  ",
        ];
        for case in 0..300 {
            let mut lines: Vec<String> = Vec::new();
            for _ in 0..(1 + next(5)) {
                let mut l = ">".repeat(next(3) as usize);
                if !l.is_empty() {
                    l.push(' ');
                }
                for _ in 0..next(40) {
                    l.push_str(vocab[next(vocab.len() as u64) as usize]);
                    l.push(' ');
                }
                lines.push(l);
            }
            let x = lines.join(if case % 2 == 0 { "\n" } else { "\r\n" });
            let wire = encode(&x, WRAP_WIDTH);
            for l in wire.lines() {
                // Over-width only when a word truly cannot be broken.
                assert!(
                    l.chars().count() <= WRAP_WIDTH
                        || !l.trim_end().trim_start_matches(['>', ' ']).contains(' ')
                        || l.contains(&"x".repeat(90)),
                    "case {case}: {l:?}"
                );
            }
            assert_round_trip_lossy(&x, &wire);
        }
    }

    /// As `assert_round_trip` but takes the precomputed wire. Lines whose
    /// quote markers have no following space (`>` + word) normalise, so the
    /// generator always emits `> `; inner multiple spaces survive.
    fn assert_round_trip_lossy(x: &str, wire: &str) {
        let expect: Vec<String> = x
            .split('\n')
            .map(|l| {
                let l = l.strip_suffix('\r').unwrap_or(l);
                let l = l.trim_end_matches(' ');
                // `decode` re-emits depth markers bare when the body is empty.
                l.to_string()
            })
            .collect();
        assert_eq!(decode(wire, false), expect.join("\n"), "wire: {wire:?}");
    }
}
