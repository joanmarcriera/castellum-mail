// Castellum - fenced-code syntax highlighting for Markdown mail.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Usage: `highlight_code(lang, code) -> Option<String>` returns the inner HTML
// of a `<pre><code>` block: the code HTML-escaped, with comments / strings /
// numbers / keywords wrapped in `<span style="color:...">`. `None` means "no
// highlighting" (unknown language, oversize input) and the caller keeps the
// plain block.
//
// Design: a tiny hand-written lexer (no dependency, no JS, no network, no
// regex) because the OUTPUT must be inline-styled HTML that survives any
// recipient client. The style strings are the fixed constants below; user text
// only ever appears HTML-escaped as span content. `token_styles()` exposes the
// constants so the compose sanitiser can allowlist exactly those values.
// Colours are mid-tones chosen to stay legible on both the themed light
// `#f5f5f7` block and an unthemed (possibly dark) background.

/// Inputs larger than this are left unhighlighted (bounded work per render;
/// the live preview re-renders on every keystroke).
const MAX_HIGHLIGHT_BYTES: usize = 64 * 1024;

const STYLE_KEYWORD: &str = "color:#c2338a";
const STYLE_STRING: &str = "color:#2f8f4e";
const STYLE_COMMENT: &str = "color:#7a828c";
const STYLE_NUMBER: &str = "color:#2f6fd0";

/// The complete set of `style` values this module can emit.
pub fn token_styles() -> [&'static str; 4] {
    [STYLE_KEYWORD, STYLE_STRING, STYLE_COMMENT, STYLE_NUMBER]
}

#[derive(Clone, Copy)]
enum Kind {
    Plain,
    Keyword,
    Str,
    Comment,
    Number,
}

/// Per-language lexical description.
struct Lang {
    keywords: &'static [&'static str],
    /// Line-comment openers.
    line_comments: &'static [&'static str],
    /// `/* ... */` comments.
    block_comments: bool,
    /// Characters that open a string (closed by the same char).
    quotes: &'static [char],
    /// `"""`/`'''` triple-quoted strings (Python).
    triple_quotes: bool,
    /// Backtick strings may span lines (JS template literals, Go raw).
    multiline_backtick: bool,
    case_insensitive: bool,
}

const C_LIKE: Lang = Lang {
    keywords: &[],
    line_comments: &["//"],
    block_comments: true,
    quotes: &['"', '\''],
    triple_quotes: false,
    multiline_backtick: false,
    case_insensitive: false,
};

fn lang_for(name: &str) -> Option<Lang> {
    let n = name.trim().to_ascii_lowercase();
    Some(match n.as_str() {
        "rust" | "rs" => Lang {
            keywords: &[
                "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else",
                "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match",
                "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct",
                "super", "trait", "true", "type", "unsafe", "use", "where", "while",
            ],
            // `'` is mostly lifetimes in Rust; only double quotes are strings.
            quotes: &['"'],
            ..C_LIKE
        },
        "swift" => Lang {
            keywords: &[
                "actor",
                "as",
                "async",
                "await",
                "break",
                "case",
                "catch",
                "class",
                "continue",
                "default",
                "defer",
                "do",
                "else",
                "enum",
                "extension",
                "false",
                "func",
                "guard",
                "if",
                "import",
                "in",
                "init",
                "let",
                "nil",
                "private",
                "protocol",
                "public",
                "return",
                "self",
                "static",
                "struct",
                "switch",
                "throw",
                "throws",
                "true",
                "try",
                "var",
                "where",
                "while",
            ],
            quotes: &['"'],
            ..C_LIKE
        },
        "go" | "golang" => Lang {
            keywords: &[
                "break",
                "case",
                "chan",
                "const",
                "continue",
                "default",
                "defer",
                "else",
                "false",
                "for",
                "func",
                "go",
                "if",
                "import",
                "interface",
                "map",
                "nil",
                "package",
                "range",
                "return",
                "select",
                "struct",
                "switch",
                "true",
                "type",
                "var",
            ],
            multiline_backtick: true,
            quotes: &['"', '\'', '`'],
            ..C_LIKE
        },
        "javascript" | "js" | "jsx" | "typescript" | "ts" | "tsx" => Lang {
            keywords: &[
                "async",
                "await",
                "break",
                "case",
                "catch",
                "class",
                "const",
                "continue",
                "default",
                "else",
                "export",
                "extends",
                "false",
                "finally",
                "for",
                "from",
                "function",
                "if",
                "import",
                "in",
                "interface",
                "let",
                "new",
                "null",
                "of",
                "return",
                "static",
                "super",
                "switch",
                "this",
                "throw",
                "true",
                "try",
                "type",
                "typeof",
                "undefined",
                "var",
                "while",
                "yield",
            ],
            multiline_backtick: true,
            quotes: &['"', '\'', '`'],
            ..C_LIKE
        },
        "c" | "h" | "cpp" | "c++" | "cc" | "hpp" | "java" | "kotlin" | "kt" | "csharp" | "cs" => {
            Lang {
                keywords: &[
                    "break",
                    "case",
                    "catch",
                    "char",
                    "class",
                    "const",
                    "continue",
                    "default",
                    "do",
                    "double",
                    "else",
                    "enum",
                    "false",
                    "final",
                    "float",
                    "for",
                    "if",
                    "int",
                    "long",
                    "namespace",
                    "new",
                    "null",
                    "nullptr",
                    "private",
                    "public",
                    "return",
                    "short",
                    "signed",
                    "sizeof",
                    "static",
                    "struct",
                    "switch",
                    "this",
                    "throw",
                    "true",
                    "try",
                    "typedef",
                    "unsigned",
                    "using",
                    "void",
                    "while",
                ],
                ..C_LIKE
            }
        }
        "python" | "py" => Lang {
            keywords: &[
                "and", "as", "assert", "async", "await", "break", "class", "continue", "def",
                "del", "elif", "else", "except", "False", "finally", "for", "from", "global", "if",
                "import", "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise",
                "return", "True", "try", "while", "with", "yield",
            ],
            line_comments: &["#"],
            block_comments: false,
            triple_quotes: true,
            ..C_LIKE
        },
        "bash" | "sh" | "shell" | "zsh" => Lang {
            keywords: &[
                "case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function",
                "if", "in", "local", "return", "then", "until", "while",
            ],
            line_comments: &["#"],
            block_comments: false,
            ..C_LIKE
        },
        "yaml" | "yml" | "toml" => Lang {
            keywords: &["false", "null", "true"],
            line_comments: &["#"],
            block_comments: false,
            ..C_LIKE
        },
        "json" | "jsonc" => Lang {
            keywords: &["false", "null", "true"],
            line_comments: &[],
            block_comments: false,
            quotes: &['"'],
            ..C_LIKE
        },
        "sql" => Lang {
            keywords: &[
                "and", "as", "by", "create", "delete", "desc", "distinct", "drop", "from", "group",
                "having", "in", "insert", "into", "is", "join", "left", "like", "limit", "not",
                "null", "on", "or", "order", "select", "set", "table", "update", "values", "where",
            ],
            line_comments: &["--"],
            block_comments: true,
            case_insensitive: true,
            quotes: &['\''],
            ..C_LIKE
        },
        _ => return None,
    })
}

/// Whether `lang` (a fence info-string word, e.g. `rust`) is highlightable.
pub fn is_supported(lang: &str) -> bool {
    lang_for(lang).is_some()
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Tokenise `code` into `(kind, text)` runs that concatenate back to `code`.
fn lex<'a>(lang: &Lang, code: &'a str) -> Vec<(Kind, &'a str)> {
    let mut out: Vec<(Kind, &str)> = Vec::new();
    let len = code.len();
    let mut i = 0;
    let mut plain_start = 0;
    while i < len {
        let rest = &code[i..];
        let c = rest.chars().next().unwrap_or(' ');
        let mut token: Option<(Kind, usize)> = None;
        if lang.line_comments.iter().any(|op| rest.starts_with(*op)) {
            token = Some((Kind::Comment, rest.find('\n').unwrap_or(rest.len())));
        } else if lang.block_comments && rest.starts_with("/*") {
            token = Some((
                Kind::Comment,
                rest[2..].find("*/").map_or(rest.len(), |p| p + 4),
            ));
        } else if lang.triple_quotes && (rest.starts_with("\"\"\"") || rest.starts_with("'''")) {
            let q = &rest[..3];
            token = Some((Kind::Str, rest[3..].find(q).map_or(rest.len(), |p| p + 6)));
        } else if lang.quotes.contains(&c) {
            let multiline = c == '`' && lang.multiline_backtick;
            let mut j = c.len_utf8();
            let mut it = rest[j..].chars();
            while let Some(ch) = it.next() {
                if ch == '\\' {
                    j += 1;
                    if let Some(n) = it.next() {
                        j += n.len_utf8();
                    }
                    continue;
                }
                if ch == c {
                    j += 1;
                    break;
                }
                if ch == '\n' && !multiline {
                    break; // unterminated: stop at end of line
                }
                j += ch.len_utf8();
            }
            token = Some((Kind::Str, j));
        } else if c.is_ascii_digit() {
            let n = rest
                .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '.' || ch == '_'))
                .unwrap_or(rest.len());
            token = Some((Kind::Number, n));
        } else if is_ident_start(c) {
            let n = rest
                .find(|ch: char| !is_ident_char(ch))
                .unwrap_or(rest.len());
            let word = &rest[..n];
            let is_kw = lang.keywords.iter().any(|k| {
                if lang.case_insensitive {
                    k.eq_ignore_ascii_case(word)
                } else {
                    *k == word
                }
            });
            if is_kw {
                token = Some((Kind::Keyword, n));
            } else {
                // Skip the identifier as plain text so e.g. `x1` is not a number.
                i += n;
                continue;
            }
        }
        match token {
            Some((kind, n)) if n > 0 => {
                if plain_start < i {
                    out.push((Kind::Plain, &code[plain_start..i]));
                }
                out.push((kind, &code[i..i + n]));
                i += n;
                plain_start = i;
            }
            _ => i += c.len_utf8(),
        }
    }
    if plain_start < len {
        out.push((Kind::Plain, &code[plain_start..]));
    }
    out
}

fn push_escaped(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
}

/// Highlight `code` as `lang`; see the module header. `None` = leave plain.
pub fn highlight_code(lang: &str, code: &str) -> Option<String> {
    if code.len() > MAX_HIGHLIGHT_BYTES {
        return None;
    }
    let lang = lang_for(lang)?;
    let mut out = String::with_capacity(code.len() * 2);
    for (kind, text) in lex(&lang, code) {
        let style = match kind {
            Kind::Plain => None,
            Kind::Keyword => Some(STYLE_KEYWORD),
            Kind::Str => Some(STYLE_STRING),
            Kind::Comment => Some(STYLE_COMMENT),
            Kind::Number => Some(STYLE_NUMBER),
        };
        match style {
            None => push_escaped(&mut out, text),
            Some(s) => {
                out.push_str("<span style=\"");
                out.push_str(s);
                out.push_str("\">");
                push_escaped(&mut out, text);
                out.push_str("</span>");
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_tags(h: &str) -> String {
        let mut o = String::new();
        let mut in_tag = false;
        for c in h.chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                c if !in_tag => o.push(c),
                _ => {}
            }
        }
        o.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&amp;", "&")
    }

    #[test]
    fn rust_tokens_get_fixed_inline_styles() {
        let h = highlight_code("rust", "let x = 42; // hi\nlet s = \"a<b\";").unwrap();
        assert!(h.contains(&format!("<span style=\"{STYLE_KEYWORD}\">let</span>")));
        assert!(h.contains(&format!("<span style=\"{STYLE_NUMBER}\">42</span>")));
        assert!(h.contains(&format!("<span style=\"{STYLE_COMMENT}\">// hi</span>")));
        assert!(h.contains(&format!(
            "<span style=\"{STYLE_STRING}\">&quot;a&lt;b&quot;</span>"
        )));
    }

    #[test]
    fn output_text_round_trips_for_every_language() {
        let code = "if (a < b && c) { return \"x\\\"y\"; } /* c */ // d\n# e\n'''t''' 0x1F `m\nl`";
        for l in [
            "rust", "swift", "go", "js", "ts", "c", "java", "python", "bash", "yaml", "json", "sql",
        ] {
            let h = highlight_code(l, code).unwrap();
            assert_eq!(strip_tags(&h), code, "{l}");
        }
    }

    #[test]
    fn hostile_code_is_escaped_never_live_markup() {
        let h = highlight_code(
            "js",
            "</code><script>alert(1)</script><img src=x onerror=1>",
        )
        .unwrap();
        assert!(!h.contains("<script"));
        assert!(!h.contains("<img"));
        assert!(h.contains("&lt;script&gt;"));
    }

    #[test]
    fn unknown_language_and_oversize_input_stay_plain() {
        assert!(highlight_code("brainfuck", "+++").is_none());
        assert!(highlight_code("rust", &"a".repeat(MAX_HIGHLIGHT_BYTES + 1)).is_none());
    }

    #[test]
    fn identifiers_containing_keywords_or_digits_are_not_split() {
        let h = highlight_code("rust", "letter x1 iffy").unwrap();
        assert!(!h.contains("<span"), "{h}");
    }

    #[test]
    fn python_triple_quote_and_hash_comment() {
        let h = highlight_code("py", "\"\"\"doc\nstring\"\"\"\n# note\ndef f(): pass").unwrap();
        assert!(h.contains(&format!(
            "<span style=\"{STYLE_STRING}\">&quot;&quot;&quot;doc\nstring&quot;&quot;&quot;</span>"
        )));
        assert!(h.contains(&format!("<span style=\"{STYLE_COMMENT}\"># note</span>")));
        assert!(h.contains(&format!("<span style=\"{STYLE_KEYWORD}\">def</span>")));
    }

    #[test]
    fn unterminated_input_never_panics() {
        for l in ["rust", "py", "js", "sql", "json"] {
            for c in [
                "\"abc", "/* open", "'''", "`x", "\\", "é\"ü", "", "0x", "\"\\",
            ] {
                let h = highlight_code(l, c).unwrap();
                assert_eq!(strip_tags(&h), c);
            }
        }
    }
}
