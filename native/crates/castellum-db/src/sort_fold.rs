// Castellum - text folding for sorted message lists (, slice 2).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// SQLite's NOCASE collation is ASCII-only, so "Àlex" and "Óscar" would sort
// after "Zoe". The sorted list queries therefore order by this fold instead:
// registered in SQL as `castellum_sort_fold(text, strip_prefixes)` (see
// `search_query::register_sql_functions`) and applied again in Rust when a
// page cursor is built, so the SQL and the cursor can never disagree.
//
// Known, accepted limits (v1): "ñ" sorts as "n" (not after it, as a Spanish
// dictionary would); non-Latin scripts sort by code point after folding.
// The function is only used in ORDER BY / WHERE, never stored or indexed, so
// an external `sqlite3` shell can still open the profile.

use crate::search_query::fold_text;

/// Reply/forward prefixes dropped from the front of a subject before
/// sorting, lower-case, including the colon.
const SUBJECT_PREFIXES: [&str; 6] = ["re:", "fwd:", "fw:", "rv:", "ref:", "aw:"];

/// Lower-case, strip diacritics (via [`fold_text`], shared with the
/// `from:`/`subject:` search qualifiers so search and sort fold alike) and
/// trim. With `strip_prefixes`, also repeatedly drop a leading `re:`, `fw:`,
/// `fwd:`, `rv:`, `ref:` or `aw:` (any case) plus the whitespace after it, so
/// "Re: Fwd: hola" sorts next to "hola".
///
/// Total over arbitrary UTF-8: never panics, and an empty or all-whitespace
/// input folds to the empty string.
pub fn sort_fold(s: &str, strip_prefixes: bool) -> String {
    let folded = fold_text(s);
    let mut rest = folded.trim();
    if strip_prefixes {
        'strip: loop {
            for p in SUBJECT_PREFIXES {
                if let Some(after) = rest.strip_prefix(p) {
                    rest = after.trim_start();
                    continue 'strip;
                }
            }
            break;
        }
    }
    rest.to_string()
}

#[cfg(test)]
mod tests {
    use super::sort_fold;

    /// Shared vector table: `(input, strip_prefixes, folded)`. The Swift
    /// client-side sort reproduces these exactly: the same table lives in
    /// `apple/Tests/CastellumCoreTests/MessageSortClientTests.swift`
    /// (`coreVectors`). Change both together.
    pub(crate) const VECTORS: &[(&str, bool, &str)] = &[
        ("Àlex", false, "alex"),
        ("Óscar", false, "oscar"),
        ("Ñu", false, "nu"),
        ("Zoe", false, "zoe"),
        ("L·L", false, "l·l"),
        ("ß", false, "ß"),
        ("đorđe", false, "dorde"),
        ("Łukasz", false, "lukasz"),
        ("Øystein", false, "oystein"),
        ("  padded  ", false, "padded"),
        ("RE: Fwd: RV: hola", true, "hola"),
        ("Re:Re:  hello", true, "hello"),
        ("AW: Ref: x", true, "x"),
        ("Fw: Fw: Fw: Fw: Fw: deep", true, "deep"),
        ("Re: hola", false, "re: hola"),
        ("Reunió", true, "reunio"),
        ("Fwdx: not a prefix", true, "fwdx: not a prefix"),
        ("", true, ""),
        ("   ", true, ""),
        ("re:", true, ""),
    ];

    #[test]
    fn folds_case_and_diacritics() {
        for (input, strip, want) in VECTORS {
            assert_eq!(
                sort_fold(input, *strip),
                *want,
                "input {input:?} strip {strip}"
            );
        }
    }

    #[test]
    fn decomposed_input_folds_like_composed() {
        assert_eq!(sort_fold("A\u{300}lex", false), "alex");
        assert_eq!(sort_fold("n\u{303}u", false), "nu");
    }

    #[test]
    fn strips_stacked_reply_prefixes() {
        assert_eq!(sort_fold("RE: Fwd: RV: hola", true), "hola");
    }

    #[test]
    fn empty_and_whitespace_fold_to_empty() {
        assert_eq!(sort_fold("", false), "");
        assert_eq!(sort_fold(" \t\n ", true), "");
    }

    #[test]
    fn never_panics_on_arbitrary_utf8() {
        // Lone combining marks, astral characters, a 1 MB string.
        for s in ["\u{300}\u{301}", "𝒜𝒷𝒸", "re:\u{0}re:", "\u{FEFF}Re: x"] {
            let _ = sort_fold(s, true);
            let _ = sort_fold(s, false);
        }
        let big = "Àé".repeat(500_000);
        assert_eq!(sort_fold(&big, true).chars().count(), 1_000_000);
    }
}
