// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - search query language (S39, extended by MM-2): `from:` `to:`
// `cc:` `bcc:` `subject:` `body:` `attachment:` `has:attachment`
// `before:`/`after:` `date:` `within:` `size:` `in:` `folder:`
// `is:read|unread|flagged|starred|answered|draft` `flag:...`,
// `-` negation, quoted phrases, bare free-text terms. The full grammar is
// documented in DECISIONS.md (2026-09-30, MM-2). Pure parsing + SQL/FTS5
// compilation — no database access, so every hostile-input case is a unit
// test here, not a fixture in `repo.rs`.
//
// Grammar (informal): a query is whitespace-separated tokens. A token is
// either `key:value` (value may be a quoted phrase) or a bare word/quoted
// phrase. Any token may be prefixed with `-` for negation. An unrecognised
// key, a key with an unparseable value (e.g. `before:not-a-date`, `has:foo`),
// an empty value (`from:`), or a bare `-`/`:` degrades to a literal
// free-text term rather than erroring — hostile input must never be
// rejected outright, only searched for literally (Principle 3, CLAUDE.md).

/// One structured filter extracted from the query text.
#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    From(String),
    To(String),
    Cc(String),
    Subject(String),
    HasAttachment,
    /// Exclusive upper bound, Unix epoch seconds at UTC midnight.
    Before(i64),
    /// Inclusive lower bound, Unix epoch seconds at UTC midnight.
    After(i64),
    /// A mailbox role, e.g. "inbox" (see `native/crates/castellum-core/src/diagnostics.rs` `KNOWN_ROLES`).
    In(String),
    IsRead,
    IsUnread,
    /// Covers both `is:flagged` and `is:starred` — see Assumption 3.
    IsFlagged,
    /// MM-2: `bcc:`.
    Bcc(String),
    /// MM-2: `body:` — FTS5 restricted to the body column. The value is kept
    /// as typed; tokenising happens at compile time.
    Body(String),
    /// MM-2: `attachment:` / `filename:` — attachment file name contains.
    AttachmentName(String),
    /// MM-2: `is:answered` / `flag:answered`; `flag:unanswered` negates it.
    IsAnswered(bool),
    /// MM-2: `is:draft` / `flag:draft`.
    IsDraft,
    /// MM-2: `date:` — half-open `[from, to)` in UTC epoch seconds; either
    /// end may be open (`date:..2026-03-31`, `date:2026-01-01..`).
    DateRange(Option<i64>, Option<i64>),
    /// MM-2: `within:7d` — sent within the last N seconds, relative to now
    /// at evaluation time.
    WithinSeconds(i64),
    /// MM-2: `size:>1mb` (`greater` = true) / `size:<500k`, in bytes.
    /// `inclusive` for `>=` / `<=`.
    Size {
        greater: bool,
        inclusive: bool,
        bytes: i64,
    },
    /// MM-2: `folder:Work` — the mailbox whose path or name equals the value
    /// (case-insensitive). Also how the app narrows a search to the current
    /// mailbox.
    Folder(String),
    /// MM-2 review: `path:` — the mailbox whose path equals the value
    /// exactly (case-sensitive). What the app's "Current mailbox" scope
    /// sends, so `Projects` never also matches `Clients/Projects`.
    Path(String),
    /// `tag:<name-or-keyword>` (S42) — matched against either the raw
    /// keyword or a tag definition's display name, case-insensitively.
    Tag(String),
    /// `is:muted` (S42).
    IsMuted,
    IsVip,
}

/// A free-text token (not a recognised operator): a bare word, a quoted
/// phrase, or an operator-shaped token whose key/value did not parse.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedTerm {
    pub text: String,
    pub negated: bool,
    /// Typed inside double quotes: an exact phrase, never prefix-matched.
    pub quoted: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedFilter {
    pub filter: Filter,
    pub negated: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParsedQuery {
    pub terms: Vec<ParsedTerm>,
    pub filters: Vec<ParsedFilter>,
}

/// Parses free-form search text into structured filters + free-text terms.
/// Pure and infallible: there is no error case, only degrade-to-free-text.
pub fn parse(input: &str) -> ParsedQuery {
    let mut query = ParsedQuery::default();
    for raw in tokenize(input) {
        // A token that opens with a quote is a literal phrase: `"to: do list"`
        // must be searched as text, never read as a `to:` filter.
        let classified = if raw.quoted {
            Classified::Term(raw.text.clone())
        } else {
            classify(&raw.text)
        };
        match classified {
            Classified::Filter(filter) => query.filters.push(ParsedFilter {
                filter,
                negated: raw.negated,
            }),
            Classified::Term(text) => query.terms.push(ParsedTerm {
                text,
                negated: raw.negated,
                quoted: raw.quoted,
            }),
        }
    }
    query
}

struct RawToken {
    text: String,
    negated: bool,
    /// The token opened with a double quote (a literal phrase).
    quoted: bool,
}

enum Classified {
    Filter(Filter),
    Term(String),
}

/// Splits `input` into whitespace-delimited tokens, honouring a single
/// leading `-` per token (negation) and one quoted span per token (which may
/// itself contain whitespace; `\\"` and `\\\\` escape a quote/backslash there). An unterminated quote consumes to the end of
/// the input rather than erroring — this function never panics on any `&str`.
fn tokenize(input: &str) -> Vec<RawToken> {
    let chars: Vec<char> = input.chars().collect();
    let n = chars.len();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < n {
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        let negated = chars[i] == '-';
        if negated {
            i += 1;
        }
        if i >= n {
            break; // trailing lone "-": nothing to negate, drop it
        }
        let mut buf = String::new();
        let quoted = chars[i] == '"';
        while i < n && !chars[i].is_whitespace() {
            if chars[i] == '"' {
                i += 1; // consume the opening quote
                while i < n && chars[i] != '"' {
                    // `\"` and `\\` inside a quoted span are escapes for a
                    // literal quote / backslash; any other backslash is literal.
                    if chars[i] == '\\'
                        && i + 1 < n
                        && (chars[i + 1] == '"' || chars[i + 1] == '\\')
                    {
                        i += 1;
                    }
                    buf.push(chars[i]);
                    i += 1;
                }
                if i < n {
                    i += 1; // consume the closing quote
                }
                // Unterminated quote: the inner loop already stopped at
                // i == n, nothing left to consume — falls out naturally.
                break; // a quoted span always ends the token
            }
            buf.push(chars[i]);
            i += 1;
        }
        if !buf.is_empty() {
            tokens.push(RawToken {
                text: buf,
                negated,
                quoted,
            });
        }
    }
    tokens
}

/// Classifies one already-tokenised, quote-stripped string as a structured
/// filter or a free-text term.
fn classify(raw: &str) -> Classified {
    if let Some(colon) = raw.find(':') {
        let key = &raw[..colon];
        let value = &raw[colon + 1..];
        // Only a short, all-alphabetic key introduces an operator — a bare
        // word that happens to contain a colon (a URL, a clock time typed
        // as a search term) is not one.
        if !key.is_empty() && key.chars().all(|c| c.is_ascii_alphabetic()) {
            if let Some(filter) = classify_operator(&key.to_ascii_lowercase(), value) {
                return Classified::Filter(filter);
            }
        }
    }
    Classified::Term(raw.to_string())
}

fn classify_operator(key: &str, value: &str) -> Option<Filter> {
    match key {
        "from" if !value.is_empty() => Some(Filter::From(value.to_string())),
        "to" if !value.is_empty() => Some(Filter::To(value.to_string())),
        "cc" if !value.is_empty() => Some(Filter::Cc(value.to_string())),
        "subject" if !value.is_empty() => Some(Filter::Subject(value.to_string())),
        "has" if value.eq_ignore_ascii_case("attachment") => Some(Filter::HasAttachment),
        "in" if !value.is_empty() => Some(Filter::In(value.to_string())),
        "tag" if !value.is_empty() => Some(Filter::Tag(value.to_string())),
        "is" => classify_is(value),
        "before" => parse_date_start_of_day(value).map(Filter::Before),
        "after" => parse_date_start_of_day(value).map(Filter::After),
        // ---- MM-2 ----
        "bcc" if !value.is_empty() => Some(Filter::Bcc(value.to_string())),
        // A value with no alphanumeric token can never match in FTS5; let it
        // fall back to a literal free-text term like any other bad value.
        "body" if !build_fts_query(value).is_empty() => Some(Filter::Body(value.to_string())),
        "attachment" | "filename" if !value.is_empty() => {
            Some(Filter::AttachmentName(value.to_string()))
        }
        "flag" => classify_flag(value),
        "date" => parse_date_range(value).map(|(from, to)| Filter::DateRange(from, to)),
        "within" => parse_relative_seconds(value).map(Filter::WithinSeconds),
        "size" => parse_size(value),
        "folder" | "mailbox" if !value.is_empty() => Some(Filter::Folder(value.to_string())),
        "path" if !value.is_empty() => Some(Filter::Path(value.to_string())),
        _ => None,
    }
}

/// `flag:` values: the `is:` set plus answered/unanswered/draft.
fn classify_flag(value: &str) -> Option<Filter> {
    match value.to_ascii_lowercase().as_str() {
        "read" => Some(Filter::IsRead),
        "unread" => Some(Filter::IsUnread),
        "flagged" | "starred" => Some(Filter::IsFlagged),
        "answered" | "replied" => Some(Filter::IsAnswered(true)),
        "unanswered" => Some(Filter::IsAnswered(false)),
        "draft" => Some(Filter::IsDraft),
        _ => None,
    }
}

/// `YYYY`, `YYYY-MM` or `YYYY-MM-DD` -> the half-open UTC epoch range it
/// covers. `None` for anything else (wrong shape, month 13, ...).
fn parse_period(value: &str) -> Option<(i64, i64)> {
    let parts: Vec<&str> = value.split('-').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let num = |s: &str, len: usize| -> Option<i64> {
        (s.len() == len && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    };
    let year = num(parts[0], 4)?;
    let start_end = |sy, sm, sd, ey, em, ed| {
        let a = days_from_civil(sy, sm, sd).checked_mul(86_400)?;
        let b = days_from_civil(ey, em, ed).checked_mul(86_400)?;
        Some((a, b))
    };
    match parts.len() {
        1 => start_end(year, 1, 1, year + 1, 1, 1),
        2 => {
            let month = num(parts[1], 2)?;
            if !(1..=12).contains(&month) {
                return None;
            }
            let (ey, em) = if month == 12 {
                (year + 1, 1)
            } else {
                (year, month + 1)
            };
            start_end(year, month, 1, ey, em, 1)
        }
        _ => {
            let month = num(parts[1], 2)?;
            let day = num(parts[2], 2)?;
            if !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
                return None;
            }
            let a = days_from_civil(year, month, day).checked_mul(86_400)?;
            Some((a, a.checked_add(86_400)?))
        }
    }
}

/// `date:` value: a single period, or `a..b` with either side optional. A
/// range end is inclusive at its own granularity (`..2026-03` includes all of
/// March). Both sides empty, or any unparseable side, is not a date filter.
fn parse_date_range(value: &str) -> Option<(Option<i64>, Option<i64>)> {
    if let Some((a, b)) = value.split_once("..") {
        let from = if a.is_empty() {
            None
        } else {
            Some(parse_period(a)?.0)
        };
        let to = if b.is_empty() {
            None
        } else {
            Some(parse_period(b)?.1)
        };
        if from.is_none() && to.is_none() {
            return None;
        }
        return Some((from, to));
    }
    let (a, b) = parse_period(value)?;
    Some((Some(a), Some(b)))
}

/// `7d`, `2w`, `3m` (30 days), `1y` (365 days) -> seconds. 1-4 digits, at
/// least 1.
fn parse_relative_seconds(value: &str) -> Option<i64> {
    let unit = value.chars().last()?.to_ascii_lowercase();
    let digits = &value[..value.len() - unit.len_utf8()];
    if digits.is_empty() || digits.len() > 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: i64 = digits.parse().ok()?;
    if n == 0 {
        return None;
    }
    let per = match unit {
        'd' => 86_400,
        'w' => 7 * 86_400,
        'm' => 30 * 86_400,
        'y' => 365 * 86_400,
        _ => return None,
    };
    n.checked_mul(per)
}

/// `>1mb`, `<500k`, `>=2g`, `<=100` (bytes). Units b/k/kb/m/mb/g/gb, 1024-based.
fn parse_size(value: &str) -> Option<Filter> {
    let (greater, inclusive, rest) = value
        .strip_prefix(">=")
        .map(|r| (true, true, r))
        .or_else(|| value.strip_prefix("<=").map(|r| (false, true, r)))
        .or_else(|| value.strip_prefix('>').map(|r| (true, false, r)))
        .or_else(|| value.strip_prefix('<').map(|r| (false, false, r)))?;
    let rest = rest.to_ascii_lowercase();
    let split = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let (digits, unit) = rest.split_at(split);
    if digits.is_empty() || digits.len() > 12 {
        return None;
    }
    let n: i64 = digits.parse().ok()?;
    let mult: i64 = match unit {
        "" | "b" => 1,
        "k" | "kb" => 1024,
        "m" | "mb" => 1024 * 1024,
        "g" | "gb" => 1024 * 1024 * 1024,
        _ => return None,
    };
    Some(Filter::Size {
        greater,
        inclusive,
        bytes: n.checked_mul(mult)?,
    })
}

fn classify_is(value: &str) -> Option<Filter> {
    match value.to_ascii_lowercase().as_str() {
        "read" => Some(Filter::IsRead),
        "unread" => Some(Filter::IsUnread),
        "flagged" | "starred" => Some(Filter::IsFlagged),
        "answered" => Some(Filter::IsAnswered(true)),
        "draft" => Some(Filter::IsDraft),
        "muted" => Some(Filter::IsMuted),
        "vip" => Some(Filter::IsVip),
        _ => None, // is:bogus falls back to a free-text term
    }
}

/// Parses a strict `YYYY-MM-DD` value into Unix epoch seconds at UTC
/// midnight. Returns `None` for anything else (wrong length/separators,
/// non-numeric, out-of-range month/day) so the whole token falls back to a
/// free-text term rather than being rejected (Assumption 5). Validates
/// month 1-12 and day 1-31 but not day-of-month validity, e.g.
/// `2027-02-30` is accepted (Assumption 6) — deliberately: a full civil
/// calendar table is more code than this stage's hostile-input contract
/// requires (accept-and-compute, never crash or reject).
fn parse_date_start_of_day(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year: i64 = value.get(0..4)?.parse().ok()?;
    let month: i64 = value.get(5..7)?.parse().ok()?;
    let day: i64 = value.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
        return None;
    }
    days_from_civil(year, month, day).checked_mul(86_400)
}

/// Number of days in `month` (1-12) of `year` (proleptic Gregorian), so
/// `2026-02-31` is rejected instead of rolling over into March.
fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Howard Hinnant's `days_from_civil`: proleptic-Gregorian calendar date ->
/// days since the Unix epoch (1970-01-01). No external date crate exists in
/// this workspace and this is the only place that needs one. All arithmetic
/// is on `i64`; `checked_mul` above guards the final seconds conversion
/// against an absurd input year overflowing, so this never panics.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

use rusqlite::types::Value;

/// A compiled query, ready to splice into a `SELECT ... FROM messages m ...`
/// statement (see `Database::search_messages_scoped` in `repo.rs`). Building
/// this never touches SQLite — every shape here is unit-tested directly.
#[derive(Debug, Default)]
pub struct CompiledQuery {
    /// Structured-filter WHERE fragments, already ANDed together. Empty
    /// when there are no filters and no negated free-text terms.
    pub where_sql: String,
    /// Bound parameters for `where_sql`'s `?`s, in order.
    pub where_params: Vec<Value>,
    /// `Some(expr)` when there is at least one *positive* free-text term —
    /// the caller JOINs `message_fts` and binds this to `MATCH ?`. `None`
    /// means skip the FTS5 join entirely (an all-filters or all-negated
    /// query has nothing positive to rank by).
    pub match_expr: Option<String>,
}

impl CompiledQuery {
    /// Whether `ORDER BY bm25(message_fts)` is meaningful for this query.
    pub fn ranked(&self) -> bool {
        self.match_expr.is_some()
    }
}

/// Compiles a `ParsedQuery` into SQL fragments + FTS5 MATCH text.
pub fn compile(query: &ParsedQuery) -> CompiledQuery {
    let mut clauses = Vec::new();
    let mut params = Vec::new();

    for f in &query.filters {
        let (clause, filter_params) = compile_filter(&f.filter);
        clauses.push(if f.negated && may_be_null(&f.filter) {
            // `-date:2026` must keep UNDATED mail: NOT(NULL) is NULL, which
            // would silently drop it, so fold NULL to "does not match".
            format!("NOT COALESCE(({clause}), 0)")
        } else if f.negated {
            format!("NOT ({clause})")
        } else {
            clause
        });
        params.extend(filter_params);
    }

    let mut positive_tokens = Vec::new();
    let mut negative_tokens = Vec::new();
    for t in &query.terms {
        // G10 (#3633): a bare positive word matches as a prefix, so "invo"
        // finds "invoice" and results appear while typing. A quoted phrase
        // stays exact, and a negated term stays exact so `-spam` never
        // hides "spamassassin" reports.
        let fts = if t.quoted || t.negated {
            build_fts_phrase(&t.text)
        } else {
            build_fts_prefix_phrase(&t.text)
        };
        if fts.is_empty() {
            continue; // pure punctuation (e.g. "---") contributes no token
        }
        if t.negated {
            negative_tokens.push(fts);
        } else {
            positive_tokens.push(fts);
        }
    }

    // A negated free-text term can't live inside the positive MATCH
    // expression (FTS5 rejects an all-NOT query, and mixing AND/NOT
    // precedence in one MATCH string is fragile) — express each one as its
    // own subquery instead, independent of whether a positive MATCH exists.
    // ONE subquery PER TERM, never a joined MATCH string: FTS5 reads adjacent
    // phrases as AND, so "spam junk" in a single NOT IN would exclude only
    // the messages carrying BOTH words, while `-spam -junk` must exclude a
    // message carrying EITHER.
    for token in &negative_tokens {
        clauses.push(
            "m.id NOT IN (SELECT message_id FROM message_fts WHERE message_fts MATCH ?)"
                .to_string(),
        );
        params.push(Value::Text(token.clone()));
    }

    CompiledQuery {
        where_sql: clauses.join(" AND "),
        where_params: params,
        match_expr: if positive_tokens.is_empty() {
            None
        } else {
            Some(positive_tokens.join(" "))
        },
    }
}

/// Filters whose clause is NULL (not false) for a message lacking the column
/// they test: date/size, `before:`/`after:` (NULL `sent_at`) and `subject:`
/// (NULL subject). Negated, these must keep such mail rather than drop it
/// (#3376; positive forms still never match NULL).
fn may_be_null(f: &Filter) -> bool {
    matches!(
        f,
        Filter::DateRange(..)
            | Filter::WithinSeconds(_)
            | Filter::Size { .. }
            | Filter::Subject(_)
            | Filter::Before(_)
            | Filter::After(_)
    )
}

fn compile_filter(f: &Filter) -> (String, Vec<Value>) {
    match f {
        Filter::From(v) => addr_clause("from", v),
        Filter::To(v) => addr_clause("to", v),
        Filter::Cc(v) => addr_clause("cc", v),
        // messages.subject LIKE, not the FTS5 subject column — see
        // Assumption 2 (keeps every structured filter on one escaping path).
        Filter::Subject(v) => (
            "cfold(m.subject) LIKE ? ESCAPE '\\'".to_string(),
            vec![Value::Text(like_pattern(&fold_text(v)))],
        ),
        Filter::HasAttachment => ("m.has_attachments = 1".to_string(), vec![]),
        // NULL sent_at (undated mail) never matches either bound — normal
        // SQL NULL-comparison semantics, not special-cased.
        Filter::Before(epoch) => ("m.sent_at < ?".to_string(), vec![Value::Integer(*epoch)]),
        Filter::After(epoch) => ("m.sent_at >= ?".to_string(), vec![Value::Integer(*epoch)]),
        Filter::In(role) => (
            "m.mailbox_id IN (SELECT id FROM mailboxes WHERE LOWER(role) = ?)".to_string(),
            vec![Value::Text(role.to_ascii_lowercase())],
        ),
        Filter::IsRead => (
            "COALESCE((SELECT is_read FROM message_flags WHERE message_id = m.id), 0) = 1"
                .to_string(),
            vec![],
        ),
        Filter::IsUnread => (
            "COALESCE((SELECT is_read FROM message_flags WHERE message_id = m.id), 0) = 0"
                .to_string(),
            vec![],
        ),
        Filter::IsFlagged => (
            "COALESCE((SELECT is_starred FROM message_flags WHERE message_id = m.id), 0) = 1"
                .to_string(),
            vec![],
        ),
        Filter::Bcc(v) => addr_clause("bcc", v),
        Filter::Body(v) => (
            // One FTS column only, so "body:x" never matches the subject.
            "m.id IN (SELECT message_id FROM message_fts WHERE message_fts MATCH ?)".to_string(),
            vec![Value::Text(format!("body : ({})", build_fts_phrase(v)))],
        ),
        Filter::AttachmentName(v) => (
            "m.id IN (SELECT message_id FROM message_attachments \
             WHERE filename LIKE ? ESCAPE '\\')"
                .to_string(),
            vec![Value::Text(like_pattern(v))],
        ),
        Filter::IsAnswered(true) => (
            "COALESCE((SELECT is_answered FROM message_flags WHERE message_id = m.id), 0) = 1"
                .to_string(),
            vec![],
        ),
        Filter::IsAnswered(false) => (
            "COALESCE((SELECT is_answered FROM message_flags WHERE message_id = m.id), 0) = 0"
                .to_string(),
            vec![],
        ),
        Filter::IsDraft => (
            "COALESCE((SELECT is_draft FROM message_flags WHERE message_id = m.id), 0) = 1"
                .to_string(),
            vec![],
        ),
        Filter::DateRange(from, to) => {
            let mut parts = Vec::new();
            let mut params = Vec::new();
            if let Some(f) = from {
                parts.push("m.sent_at >= ?");
                params.push(Value::Integer(*f));
            }
            if let Some(t) = to {
                parts.push("m.sent_at < ?");
                params.push(Value::Integer(*t));
            }
            (parts.join(" AND "), params)
        }
        Filter::WithinSeconds(secs) => (
            "m.sent_at >= CAST(strftime('%s','now') AS INTEGER) - ?".to_string(),
            vec![Value::Integer(*secs)],
        ),
        Filter::Size {
            greater,
            inclusive,
            bytes,
        } => {
            let cmp = match (greater, inclusive) {
                (true, false) => ">",
                (true, true) => ">=",
                (false, false) => "<",
                (false, true) => "<=",
            };
            (
                format!("m.size_bytes {cmp} ?"),
                vec![Value::Integer(*bytes)],
            )
        }
        Filter::Path(path) => (
            "m.mailbox_id IN (SELECT id FROM mailboxes WHERE path = ?)".to_string(),
            vec![Value::Text(path.clone())],
        ),
        Filter::Folder(name) => (
            "m.mailbox_id IN (SELECT id FROM mailboxes \
             WHERE LOWER(path) = LOWER(?) OR LOWER(name) = LOWER(?))"
                .to_string(),
            vec![Value::Text(name.clone()), Value::Text(name.clone())],
        ),
        // Matches either the raw keyword (normalised, case-insensitive by
        // construction) or a tag definition's display name (COLLATE NOCASE
        // — display names are user text, not IMAP atoms).
        Filter::Tag(name) => (
            "EXISTS (SELECT 1 FROM message_keywords mk WHERE mk.message_id = m.id \
             AND mk.keyword_key IN (SELECT ? UNION SELECT keyword_key FROM tag_definitions \
             WHERE display_name = ? COLLATE NOCASE))"
                .to_string(),
            vec![
                Value::Text(crate::tags::normalise_keyword(name)),
                Value::Text(name.clone()),
            ],
        ),
        Filter::IsMuted => (
            "COALESCE((SELECT is_muted FROM threads WHERE id = m.thread_id), 0) = 1".to_string(),
            vec![],
        ),
        Filter::IsVip => (
            "EXISTS (SELECT 1 FROM vip_senders v WHERE v.address_key = lower(trim(m.from_address)))"
                .to_string(),
            vec![],
        ),
    }
}

fn addr_clause(kind: &'static str, value: &str) -> (String, Vec<Value>) {
    // `from:=ann@x.test` (MM-2 review): a leading `=` means "this exact address"
    // (case-insensitive) instead of the default contains-match on address or
    // name — what the Correspondence view needs so `ann@x.test` does not also
    // pull in `joann@x.test`. A lone `=` stays an ordinary contains search.
    if let Some(exact) = value.strip_prefix('=').filter(|v| !v.is_empty()) {
        return (
            "EXISTS (SELECT 1 FROM message_addresses ma WHERE ma.message_id = m.id \
             AND ma.kind = ? AND LOWER(ma.address) = LOWER(?))"
                .to_string(),
            vec![
                Value::Text(kind.to_string()),
                Value::Text(exact.to_string()),
            ],
        );
    }
    (
        "EXISTS (SELECT 1 FROM message_addresses ma WHERE ma.message_id = m.id \
         AND ma.kind = ? AND (cfold(ma.address) LIKE ? ESCAPE '\\' OR cfold(ma.name) LIKE ? ESCAPE '\\'))"
            .to_string(),
        vec![
            Value::Text(kind.to_string()),
            Value::Text(like_pattern(&fold_text(value))),
            Value::Text(like_pattern(&fold_text(value))),
        ],
    )
}

/// Wraps a user value as a `%...%` LIKE pattern, escaping the three
/// characters that are special to SQL `LIKE` (`\`, `%`, `_`) so a literal
/// percent/underscore typed by the user (or pasted from an address like
/// `first_last@x.test`) is matched literally, not as a wildcard.
fn like_pattern(v: &str) -> String {
    let escaped = v
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

/// Build a safe FTS5 MATCH expression from free user text: keep alphanumeric
/// tokens, quote each as a phrase, AND them together. Prevents FTS5 syntax
/// errors/injection from punctuation or operators in the query. Moved here
/// verbatim from `repo.rs` (S39) — same function, same behaviour, now
/// reused by `compile` for both positive and negative free-text terms.
pub(crate) fn build_fts_query(input: &str) -> String {
    fts_words(input)
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Like [`build_fts_query`] but the words form ONE FTS5 phrase (`"a b c"`):
/// ordered and adjacent, which is what a quoted span in the grammar means.
/// Empty when the input holds no word. Used for free-text terms and `body:`.
pub(crate) fn build_fts_phrase(input: &str) -> String {
    let words: Vec<&str> = fts_words(input).collect();
    if words.is_empty() {
        String::new()
    } else {
        format!("\"{}\"", words.join(" "))
    }
}

/// [`build_fts_phrase`] with FTS5's prefix marker on its LAST word (`"a b"*`
/// matches "a b", "a bc"...), for bare free-text terms (G10). A one-letter
/// last word stays exact: as a prefix it would match most of the mailbox.
pub(crate) fn build_fts_prefix_phrase(input: &str) -> String {
    let phrase = build_fts_phrase(input);
    let last_len = fts_words(input).last().map_or(0, |w| w.chars().count());
    if phrase.is_empty() || last_len < 2 {
        phrase
    } else {
        format!("{phrase}*")
    }
}

/// Words of a user query. Combining marks stay inside a word, so a
/// decomposed (NFD) "ca\u{300}lcul" is one word, as the NFC form is.
fn fts_words(input: &str) -> impl Iterator<Item = &str> {
    input
        .split(|c: char| !(c.is_alphanumeric() || is_combining_mark(c)))
        .filter(|t| !t.is_empty())
}

/// Combining diacritical marks (U+0300..=U+036F plus the extended,
/// symbols and half-mark blocks).
fn is_combining_mark(c: char) -> bool {
    matches!(
        c as u32,
        0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F
    )
}

/// Case- and accent-folds `s` for the `from:`/`subject:` qualifiers, so they
/// behave like free-text search (FTS5 `unicode61`): Unicode lowercase,
/// combining marks dropped, Latin accented letters reduced to their base
/// letter. Registered in SQL as `cfold()` and also applied to the user's
/// pattern, so both sides fold identically.
pub(crate) fn fold_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().flat_map(char::to_lowercase) {
        if is_combining_mark(c) {
            continue;
        }
        out.push(match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => 'a',
            'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => 'c',
            'ď' | 'đ' => 'd',
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => 'e',
            'ĝ' | 'ğ' | 'ġ' | 'ģ' => 'g',
            'ĥ' => 'h',
            'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' => 'i',
            'ĵ' => 'j',
            'ķ' => 'k',
            'ĺ' | 'ļ' | 'ľ' | 'ł' => 'l',
            'ñ' | 'ń' | 'ņ' | 'ň' => 'n',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => 'o',
            'ŕ' | 'ŗ' | 'ř' => 'r',
            'ś' | 'ŝ' | 'ş' | 'š' => 's',
            'ţ' | 'ť' => 't',
            'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => 'u',
            'ŵ' => 'w',
            'ý' | 'ÿ' | 'ŷ' => 'y',
            'ź' | 'ż' | 'ž' => 'z',
            other => other,
        });
    }
    out
}

/// Registers the deterministic scalar `cfold(text)` (see [`fold_text`]) on a
/// connection. A SQL function is not schema: no migration. Every connection
/// that may run a compiled search must call this.
pub(crate) fn register_sql_functions(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    use rusqlite::functions::FunctionFlags;
    conn.create_scalar_function(
        "cfold",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            // NULL in, NULL out: LIKE on NULL stays "no match".
            Ok(ctx.get::<Option<String>>(0)?.map(|s| fold_text(&s)))
        },
    )?;
    // Sorted message lists (#3043): `castellum_sort_fold(text, strip_prefixes)`.
    // NULL in, empty string out, so a missing sender or subject sorts first.
    conn.create_scalar_function(
        "castellum_sort_fold",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text = ctx.get::<Option<String>>(0)?.unwrap_or_default();
            let strip = ctx.get::<i64>(1)? != 0;
            Ok(crate::sort_fold::sort_fold(&text, strip))
        },
    )
}

// ---------------------------------------------------------------------------
// Hybrid semantic + keyword search (S53)
// ---------------------------------------------------------------------------
//
// The three functions below are the pure-Rust half of hybrid search: they
// know nothing about SQLite, Ollama, or `Database` — every input is a value
// already in hand, and every case is a unit test right here rather than a
// fixture that needs a real profile. `Database::search_messages_hybrid`
// (repo.rs) is the only caller: it runs the UNCHANGED S39 keyword path
// (`search_messages_scoped`), then — only when it already has the query's
// own embedding vector in hand — uses these to fold semantic similarity
// into the ranking of exactly those keyword results. Nothing here ever
// blocks or calls out to a model: computing the query embedding is the
// caller's job, done (or skipped, on an unreachable backend) before any of
// this runs.

/// Decode a `message_embeddings.vector` BLOB (little-endian f32, per the
/// migration 0031 doc comment) into floats, validating it against the row's
/// own `dimension` column.
///
/// Returns `None` — never panics, never reads past the end of `bytes` — for
/// a BLOB whose byte length is not a multiple of 4 (can't be whole f32s), or
/// whose decoded float count disagrees with `dimension` (a stale row from a
/// different model, or corruption). A stored vector must never be trusted
/// blindly; the caller treats `None` exactly like "no embedding for this
/// message" (see `Database::search_messages_hybrid`'s degrade behaviour).
pub fn decode_vector_f32(bytes: &[u8], dimension: i64) -> Option<Vec<f32>> {
    if dimension <= 0 || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let count = (bytes.len() / 4) as i64;
    if count != dimension {
        return None;
    }
    Some(
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect(),
    )
}

/// Cosine similarity between two vectors, in `[-1.0, 1.0]`. `None` — never a
/// panic, never a division by zero — when the vectors differ in length or
/// either is the zero vector (undefined direction).
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.is_empty() || a.len() != b.len() {
        return None;
    }
    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let (x, y) = (*x as f64, *y as f64);
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return None;
    }
    // A non-finite component (inf/NaN from a misbehaving embedding backend)
    // yields NaN here; NaN would make the callers' sorts non-total (#3377).
    // Report it as "no usable embedding" instead.
    let r = (dot / (norm_a.sqrt() * norm_b.sqrt())) as f32;
    r.is_finite().then_some(r)
}

/// Combine one message's keyword rank and (optional) semantic similarity
/// into a single deterministic ordering score — higher sorts first.
///
/// Weighting (fixed, documented here because nothing else explains the
/// ranking): keyword contributes `0.7 * 1/(rank + 1)` — reciprocal rank on
/// the message's 0-based position in the keyword result list, which is
/// already ordered by `bm25(message_fts)`/`sent_at`. Semantic similarity
/// contributes `0.3 * cosine`, only when `Some` — a message with no
/// `message_embeddings` row (or a query with no computable embedding at
/// all) simply gets `0.0` from this term and keeps its keyword-only
/// position relative to other un-embedded messages. Because the keyword
/// term is a strictly decreasing function of rank and the semantic term is
/// bounded to `[-0.3, 0.3]`, a message can be pulled ahead of a same- or
/// lower-scoring keyword neighbour by a strong semantic match, but the
/// combination is total-order deterministic for a fixed input: same ranks
/// and similarities always produce the same score, and equal scores are
/// left in their original (keyword) order by the caller's stable sort.
pub fn combine_scores(keyword_rank: usize, semantic_similarity: Option<f32>) -> f64 {
    const KEYWORD_WEIGHT: f64 = 0.7;
    const SEMANTIC_WEIGHT: f64 = 0.3;
    let keyword_component = KEYWORD_WEIGHT / (keyword_rank as f64 + 1.0);
    let semantic_component = semantic_similarity
        .map(|s| SEMANTIC_WEIGHT * s.clamp(-1.0, 1.0) as f64)
        .unwrap_or(0.0);
    keyword_component + semantic_component
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(q: &ParsedQuery) -> Vec<(&str, bool)> {
        q.terms
            .iter()
            .map(|t| (t.text.as_str(), t.negated))
            .collect()
    }

    #[test]
    fn bare_words_are_terms() {
        let q = parse("budget report");
        assert_eq!(terms(&q), vec![("budget", false), ("report", false)]);
        assert!(q.filters.is_empty());
    }

    #[test]
    fn negation_prefix_is_stripped_and_recorded() {
        let q = parse("-spam budget");
        assert_eq!(terms(&q), vec![("spam", true), ("budget", false)]);
    }

    #[test]
    fn quoted_phrase_keeps_its_internal_whitespace() {
        let q = parse(r#""weekly report" budget"#);
        assert_eq!(terms(&q), vec![("weekly report", false), ("budget", false)]);
    }

    #[test]
    fn unterminated_quote_consumes_to_end_of_input_without_panicking() {
        let q = parse(r#"subject:"weekly report"#);
        // No closing quote: everything after the opening quote is the value.
        assert_eq!(
            q.filters,
            vec![ParsedFilter {
                filter: Filter::Subject("weekly report".into()),
                negated: false,
            }]
        );
    }

    #[test]
    fn lone_dash_and_lone_colon_never_panic_and_become_terms_or_nothing() {
        for q in ["-", "-  -", ":", "::::", "   ", "", "\""] {
            let _ = parse(q); // must not panic
        }
        assert!(parse("-").terms.is_empty(), "a lone dash negates nothing");
        assert_eq!(terms(&parse(":")), vec![(":", false)]);
    }

    #[test]
    fn from_to_cc_subject_are_recognised() {
        let q = parse(r#"from:alice to:bob cc:carol subject:"weekly report""#);
        assert_eq!(
            q.filters,
            vec![
                ParsedFilter {
                    filter: Filter::From("alice".into()),
                    negated: false
                },
                ParsedFilter {
                    filter: Filter::To("bob".into()),
                    negated: false
                },
                ParsedFilter {
                    filter: Filter::Cc("carol".into()),
                    negated: false
                },
                ParsedFilter {
                    filter: Filter::Subject("weekly report".into()),
                    negated: false
                },
            ]
        );
    }

    #[test]
    fn negated_operator_is_recorded_on_the_filter() {
        let q = parse("-from:spam@bad.test");
        assert_eq!(
            q.filters,
            vec![ParsedFilter {
                filter: Filter::From("spam@bad.test".into()),
                negated: true,
            }]
        );
    }

    #[test]
    fn empty_value_falls_back_to_a_literal_term() {
        // "from:" alone would otherwise compile to a LIKE '%%' that matches
        // every message — degrade to searching for the literal text instead.
        let q = parse("from:");
        assert!(q.filters.is_empty());
        assert_eq!(terms(&q), vec![("from:", false)]);
    }

    #[test]
    fn a_word_containing_a_colon_that_is_not_a_known_key_is_a_bare_term() {
        let q = parse("http://example.test 10:30am");
        assert_eq!(
            terms(&q),
            vec![("http://example.test", false), ("10:30am", false)]
        );
    }

    #[test]
    fn unicode_input_never_panics() {
        let _ = parse("café münchën 日本語 -\"quöted phräse\"");
    }

    #[test]
    fn has_attachment_is_case_insensitive_other_values_fall_back() {
        assert_eq!(
            parse("has:attachment").filters,
            vec![ParsedFilter {
                filter: Filter::HasAttachment,
                negated: false,
            }]
        );
        assert_eq!(
            parse("HAS:ATTACHMENT").filters,
            vec![ParsedFilter {
                filter: Filter::HasAttachment,
                negated: false,
            }]
        );
        let q = parse("has:calendar");
        assert!(q.filters.is_empty());
        assert_eq!(terms(&q), vec![("has:calendar", false)]);
    }

    #[test]
    fn in_matches_any_role_value_unvalidated() {
        assert_eq!(
            parse("in:inbox").filters,
            vec![ParsedFilter {
                filter: Filter::In("inbox".into()),
                negated: false,
            }]
        );
        // Not in KNOWN_ROLES — still a structured filter, just one that
        // will match zero mailboxes at query time (Assumption 4).
        assert_eq!(
            parse("in:archived").filters,
            vec![ParsedFilter {
                filter: Filter::In("archived".into()),
                negated: false,
            }]
        );
    }

    #[test]
    fn is_read_unread_flagged_starred_are_recognised_bogus_falls_back() {
        assert_eq!(parse("is:read").filters[0].filter, Filter::IsRead);
        assert_eq!(parse("is:unread").filters[0].filter, Filter::IsUnread);
        assert_eq!(parse("is:flagged").filters[0].filter, Filter::IsFlagged);
        assert_eq!(parse("is:starred").filters[0].filter, Filter::IsFlagged);
        let q = parse("is:archived");
        assert!(q.filters.is_empty());
        assert_eq!(terms(&q), vec![("is:archived", false)]);
    }

    #[test]
    fn negated_has_in_is_are_recorded() {
        assert!(parse("-has:attachment").filters[0].negated);
        assert!(parse("-in:trash").filters[0].negated);
        assert!(parse("-is:unread").filters[0].negated);
    }

    #[test]
    fn known_epoch_dates_convert_exactly() {
        // 1970-01-01 00:00:00 UTC is epoch 0 by construction.
        assert_eq!(parse_date_start_of_day("1970-01-01"), Some(0));
        // 2000-01-01 00:00:00 UTC is the well-known Unix time 946684800.
        assert_eq!(parse_date_start_of_day("2000-01-01"), Some(946_684_800));
    }

    #[test]
    fn before_after_recognised_garbage_falls_back_to_free_text() {
        // 2026-09-18 00:00:00 UTC (verified against Python's
        // datetime.datetime(2026, 9, 18, tzinfo=timezone.utc).timestamp()) —
        // the plan's literal here was computed for 2025, not 2026; fixed as
        // a deviation, see the plan's Deviations list.
        assert_eq!(
            parse("before:2026-09-18").filters[0].filter,
            Filter::Before(1_789_689_600)
        );
        assert_eq!(
            parse("after:2026-09-18").filters[0].filter,
            Filter::After(1_789_689_600)
        );
        for bad in [
            "before:2026",
            "before:2026/09/18",
            "before:not-a-date",
            "before:9999999999999-01-01",
        ] {
            let q = parse(bad);
            assert!(q.filters.is_empty(), "{bad:?} must not parse as a date");
            assert_eq!(terms(&q), vec![(bad, false)]);
        }
    }

    #[test]
    fn date_parser_never_panics_on_hostile_input() {
        for bad in [
            "before:",
            "before:----------",
            "before:99-99-99",
            "before:\u{0}\u{0}\u{0}\u{0}-00-00",
            "after:-1-01-01",
        ] {
            let _ = parse(bad); // must not panic, overflow-panic included
        }
    }

    fn text(v: &Value) -> &str {
        match v {
            Value::Text(s) => s.as_str(),
            other => panic!("expected Value::Text, got {other:?}"),
        }
    }

    #[test]
    fn bare_keywords_compile_to_the_legacy_fts_shape_only() {
        let c = compile(&parse("budget report"));
        assert_eq!(c.where_sql, "");
        assert!(c.where_params.is_empty());
        assert_eq!(c.match_expr.as_deref(), Some("\"budget\"* \"report\"*"));
        assert!(c.ranked());
    }

    #[test]
    fn blank_query_compiles_to_nothing() {
        let c = compile(&parse("   "));
        assert_eq!(c.where_sql, "");
        assert_eq!(c.match_expr, None);
        assert!(!c.ranked());
    }

    #[test]
    fn from_compiles_to_an_exists_clause_with_escaped_like_params() {
        let c = compile(&parse("from:alice_b%40x.test"));
        assert_eq!(c.where_sql, "EXISTS (SELECT 1 FROM message_addresses ma WHERE ma.message_id = m.id AND ma.kind = ? AND (cfold(ma.address) LIKE ? ESCAPE '\\' OR cfold(ma.name) LIKE ? ESCAPE '\\'))");
        assert_eq!(text(&c.where_params[0]), "from");
        // Both LIKE special characters in the value are escaped — the "_"
        // as well as the "%" (the plan's literal here escaped only "%";
        // fixed as a deviation, see the plan's Deviations list — leaving
        // "_" unescaped would make it a single-char wildcard, matching e.g.
        // "aliceXb%40x.test" too, which like_pattern's own doc comment says
        // must not happen).
        assert_eq!(text(&c.where_params[1]), "%alice\\_b\\%40x.test%");
        assert_eq!(c.match_expr, None);
    }

    #[test]
    fn has_attachment_and_in_and_is_compile_to_plain_predicates() {
        let c = compile(&parse("has:attachment in:inbox is:unread"));
        assert_eq!(
            c.where_sql,
            "m.has_attachments = 1 AND m.mailbox_id IN (SELECT id FROM mailboxes WHERE LOWER(role) = ?) AND COALESCE((SELECT is_read FROM message_flags WHERE message_id = m.id), 0) = 0"
        );
        assert_eq!(text(&c.where_params[0]), "inbox");
        assert_eq!(c.match_expr, None);
    }

    #[test]
    fn negated_filter_is_wrapped_in_not() {
        let c = compile(&parse("-has:attachment"));
        assert_eq!(c.where_sql, "NOT (m.has_attachments = 1)");
    }

    #[test]
    fn negated_free_text_term_compiles_to_a_not_in_subquery() {
        let c = compile(&parse("-spam"));
        assert_eq!(
            c.where_sql,
            "m.id NOT IN (SELECT message_id FROM message_fts WHERE message_fts MATCH ?)"
        );
        assert_eq!(text(&c.where_params[0]), "\"spam\"");
        assert_eq!(
            c.match_expr, None,
            "an all-negated query has nothing positive to rank by"
        );
    }

    #[test]
    fn each_negated_term_gets_its_own_subquery() {
        // `-spam -junk` must exclude a message carrying EITHER word. Joining
        // both into one MATCH string would AND them inside FTS5 and exclude
        // only the messages carrying both.
        let c = compile(&parse("-spam -junk"));
        assert_eq!(
            c.where_sql,
            "m.id NOT IN (SELECT message_id FROM message_fts WHERE message_fts MATCH ?) \
             AND m.id NOT IN (SELECT message_id FROM message_fts WHERE message_fts MATCH ?)"
        );
        assert_eq!(text(&c.where_params[0]), "\"spam\"");
        assert_eq!(text(&c.where_params[1]), "\"junk\"");
    }

    #[test]
    fn combined_query_compiles_filters_positive_and_negative_terms_together() {
        let c = compile(&parse("budget -spam from:alice"));
        assert!(c.where_sql.starts_with("EXISTS"));
        assert!(c.where_sql.contains("m.id NOT IN"));
        assert_eq!(c.match_expr.as_deref(), Some("\"budget\"*"));
        assert!(c.ranked());
    }

    /// G10 (#3633): bare positive words are prefixes; quoted phrases,
    /// negated words and one-letter words stay exact.
    #[test]
    fn bare_words_are_prefixes_but_phrases_negations_and_single_letters_are_exact() {
        assert_eq!(
            compile(&parse("invo")).match_expr.as_deref(),
            Some("\"invo\"*")
        );
        assert_eq!(
            compile(&parse(r#""quick brown""#)).match_expr.as_deref(),
            Some("\"quick brown\"")
        );
        assert_eq!(compile(&parse("a")).match_expr.as_deref(), Some("\"a\""));
        // first.last splits into one phrase; only its last word is a prefix.
        assert_eq!(
            compile(&parse("first.la")).match_expr.as_deref(),
            Some("\"first la\"*")
        );
        let neg = compile(&parse("budget -spam"));
        assert_eq!(text(&neg.where_params[0]), "\"spam\"");
        assert_eq!(build_fts_prefix_phrase("---"), "");
    }

    // -- hybrid search (S53) ------------------------------------------------

    fn le_bytes(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|f| f.to_le_bytes()).collect()
    }

    #[test]
    fn decode_vector_f32_round_trips_a_well_formed_blob() {
        let v = vec![1.0f32, -2.5, 0.0, 3.25];
        let bytes = le_bytes(&v);
        assert_eq!(decode_vector_f32(&bytes, 4), Some(v));
    }

    #[test]
    fn decode_vector_f32_rejects_a_length_not_a_multiple_of_four() {
        // 5 bytes: not a whole number of f32s. Must return None, never panic
        // (in particular, never read the trailing partial chunk).
        let bytes = vec![0u8, 1, 2, 3, 4];
        assert_eq!(decode_vector_f32(&bytes, 1), None);
        // Also true when the truncated length happens to still equal the
        // claimed dimension in bytes-per-float terms.
        assert_eq!(decode_vector_f32(&[0u8; 6], 1), None);
    }

    #[test]
    fn decode_vector_f32_rejects_a_dimension_mismatch() {
        // 4 well-formed floats (16 bytes) but the row claims dimension 3 —
        // e.g. a stale row from a different model. Must return None.
        let bytes = le_bytes(&[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(decode_vector_f32(&bytes, 3), None);
        assert_eq!(decode_vector_f32(&bytes, 5), None);
    }

    #[test]
    fn decode_vector_f32_rejects_a_non_positive_dimension() {
        assert_eq!(decode_vector_f32(&le_bytes(&[1.0]), 0), None);
        assert_eq!(decode_vector_f32(&le_bytes(&[1.0]), -1), None);
    }

    #[test]
    fn decode_vector_f32_never_panics_on_arbitrary_short_input() {
        // Fuzz-lite: every length from 0 to 20 bytes, against a handful of
        // claimed dimensions, must return cleanly (Some or None), not panic.
        for len in 0..=20usize {
            let bytes = vec![0xABu8; len];
            for dim in [-1i64, 0, 1, 2, 3, 4, 5, 100] {
                let _ = decode_vector_f32(&bytes, dim);
            }
        }
    }

    #[test]
    fn cosine_similarity_of_identical_vectors_is_one() {
        let v = vec![1.0f32, 2.0, 3.0];
        let sim = cosine_similarity(&v, &v).expect("equal-length, non-zero");
        assert!((sim - 1.0).abs() < 1e-6, "got {sim}");
    }

    #[test]
    fn cosine_similarity_of_opposite_vectors_is_minus_one() {
        let a = vec![1.0f32, 0.0];
        let b = vec![-1.0f32, 0.0];
        let sim = cosine_similarity(&a, &b).unwrap();
        assert!((sim + 1.0).abs() < 1e-6, "got {sim}");
    }

    #[test]
    fn cosine_similarity_of_orthogonal_vectors_is_zero() {
        let a = vec![1.0f32, 0.0];
        let b = vec![0.0f32, 1.0];
        let sim = cosine_similarity(&a, &b).unwrap();
        assert!(sim.abs() < 1e-6, "got {sim}");
    }

    #[test]
    fn cosine_similarity_rejects_mismatched_lengths() {
        assert_eq!(cosine_similarity(&[1.0, 2.0], &[1.0]), None);
    }

    #[test]
    fn cosine_similarity_rejects_empty_and_zero_vectors() {
        assert_eq!(cosine_similarity(&[], &[]), None);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), None);
    }

    #[test]
    fn cosine_similarity_rejects_non_finite_components() {
        // #3377: inf/inf was NaN, which poisoned the sort order.
        assert_eq!(cosine_similarity(&[f32::INFINITY, 1.0], &[1.0, 1.0]), None);
        assert_eq!(cosine_similarity(&[1.0, 1.0], &[f32::NAN, 1.0]), None);
        assert_eq!(
            cosine_similarity(&[f32::NEG_INFINITY, 1.0], &[f32::INFINITY, 1.0]),
            None
        );
    }

    #[test]
    fn combine_scores_ranks_a_closer_keyword_match_higher_with_no_semantic_signal() {
        let top = combine_scores(0, None);
        let lower = combine_scores(5, None);
        assert!(top > lower);
    }

    #[test]
    fn combine_scores_lets_strong_similarity_pull_a_lower_keyword_rank_ahead() {
        // Rank 3 with a near-perfect semantic match should outscore rank 0
        // with no embedding at all (0.7/4 + 0.3*0.99 ≈ 0.472 > 0.7/1 = 0.7?
        // No — keyword weight dominates at rank 0, so pick ranks where the
        // semantic budget is actually decisive: rank 1 vs rank 2.
        let rank1_no_sim = combine_scores(1, None); // 0.7/2 = 0.35
        let rank2_high_sim = combine_scores(2, Some(0.99)); // 0.7/3 + 0.3*0.99 ≈ 0.5301
        assert!(
            rank2_high_sim > rank1_no_sim,
            "rank2_high_sim={rank2_high_sim} rank1_no_sim={rank1_no_sim}"
        );
    }

    #[test]
    fn combine_scores_is_deterministic_for_fixed_input() {
        assert_eq!(combine_scores(2, Some(0.42)), combine_scores(2, Some(0.42)));
        let a = combine_scores(4, Some(-0.2));
        let b = combine_scores(4, Some(-0.2));
        assert_eq!(a, b);
    }

    #[test]
    fn combine_scores_clamps_out_of_range_similarity() {
        // A malformed/impossible similarity outside [-1, 1] must not blow
        // the semantic term past its 0.3 budget in either direction.
        let clamped_high = combine_scores(0, Some(5.0));
        let at_one = combine_scores(0, Some(1.0));
        assert_eq!(clamped_high, at_one);
        let clamped_low = combine_scores(0, Some(-5.0));
        let at_minus_one = combine_scores(0, Some(-1.0));
        assert_eq!(clamped_low, at_minus_one);
    }

    // ---- MM-2 qualifiers ----

    fn one(q: &str) -> (Filter, bool) {
        let p = parse(q);
        assert!(
            p.terms.is_empty(),
            "{q}: unexpected free-text terms {:?}",
            p.terms
        );
        assert_eq!(p.filters.len(), 1, "{q}");
        (p.filters[0].filter.clone(), p.filters[0].negated)
    }

    #[test]
    fn mm2_bcc_body_attachment_folder_parse() {
        assert_eq!(one("bcc:boss@x.test").0, Filter::Bcc("boss@x.test".into()));
        assert_eq!(
            one("body:\"quarterly report\"").0,
            Filter::Body("quarterly report".into())
        );
        assert_eq!(
            one("attachment:invoice.pdf").0,
            Filter::AttachmentName("invoice.pdf".into())
        );
        assert_eq!(one("filename:x").0, Filter::AttachmentName("x".into()));
        assert_eq!(one("folder:Work").0, Filter::Folder("Work".into()));
        assert_eq!(one("-folder:Spam"), (Filter::Folder("Spam".into()), true));
    }

    #[test]
    fn mm2_flag_and_is_extras() {
        assert_eq!(one("flag:starred").0, Filter::IsFlagged);
        assert_eq!(one("flag:unread").0, Filter::IsUnread);
        assert_eq!(one("flag:answered").0, Filter::IsAnswered(true));
        assert_eq!(one("flag:unanswered").0, Filter::IsAnswered(false));
        assert_eq!(one("flag:draft").0, Filter::IsDraft);
        assert_eq!(one("is:answered").0, Filter::IsAnswered(true));
        assert_eq!(one("is:draft").0, Filter::IsDraft);
        // bad flag value degrades to a literal term
        let p = parse("flag:nonsense");
        assert!(p.filters.is_empty());
        assert_eq!(p.terms[0].text, "flag:nonsense");
    }

    #[test]
    fn mm2_date_periods_and_ranges() {
        let day = |y, m, d| days_from_civil(y, m, d) * 86_400;
        assert_eq!(
            one("date:2026").0,
            Filter::DateRange(Some(day(2026, 1, 1)), Some(day(2027, 1, 1)))
        );
        assert_eq!(
            one("date:2026-03").0,
            Filter::DateRange(Some(day(2026, 3, 1)), Some(day(2026, 4, 1)))
        );
        assert_eq!(
            one("date:2026-12").0,
            Filter::DateRange(Some(day(2026, 12, 1)), Some(day(2027, 1, 1)))
        );
        assert_eq!(
            one("date:2026-03-15").0,
            Filter::DateRange(Some(day(2026, 3, 15)), Some(day(2026, 3, 16)))
        );
        assert_eq!(
            one("date:2026-01-01..2026-03-31").0,
            Filter::DateRange(Some(day(2026, 1, 1)), Some(day(2026, 4, 1)))
        );
        assert_eq!(
            one("date:..2026-03").0,
            Filter::DateRange(None, Some(day(2026, 4, 1)))
        );
        assert_eq!(
            one("date:2026-06..").0,
            Filter::DateRange(Some(day(2026, 6, 1)), None)
        );
        for bad in [
            "date:",
            "date:..",
            "date:2026-13",
            "date:26",
            "date:2026-1",
            "date:2026-03-15-01",
            "date:abc",
            "date:2026..x",
        ] {
            assert!(
                parse(bad).filters.is_empty(),
                "{bad} should degrade to a term"
            );
        }
    }

    #[test]
    fn mm2_within_and_size() {
        assert_eq!(one("within:7d").0, Filter::WithinSeconds(7 * 86_400));
        assert_eq!(one("within:2w").0, Filter::WithinSeconds(14 * 86_400));
        assert_eq!(one("within:3m").0, Filter::WithinSeconds(90 * 86_400));
        assert_eq!(one("within:1y").0, Filter::WithinSeconds(365 * 86_400));
        for bad in [
            "within:0d",
            "within:d",
            "within:7",
            "within:7x",
            "within:99999d",
            "within:-1d",
        ] {
            assert!(parse(bad).filters.is_empty(), "{bad}");
        }
        assert_eq!(
            one("size:>1mb").0,
            Filter::Size {
                greater: true,
                inclusive: false,
                bytes: 1024 * 1024
            }
        );
        assert_eq!(
            one("size:<500k").0,
            Filter::Size {
                greater: false,
                inclusive: false,
                bytes: 500 * 1024
            }
        );
        assert_eq!(
            one("size:>=2gb").0,
            Filter::Size {
                greater: true,
                inclusive: true,
                bytes: 2 * 1024 * 1024 * 1024
            }
        );
        assert_eq!(
            one("size:<=100").0,
            Filter::Size {
                greater: false,
                inclusive: true,
                bytes: 100
            }
        );
        for bad in [
            "size:1mb",
            "size:>",
            "size:>mb",
            "size:>1xb",
            "size:>99999999999999",
        ] {
            assert!(parse(bad).filters.is_empty(), "{bad}");
        }
    }

    #[test]
    fn mm2_tag_combines_with_mm2_qualifiers() {
        // tag: was reserved while PR #173 was open; it now parses beside them.
        let p = parse("tag:work flag:answered");
        assert_eq!(
            p.filters.iter().map(|f| &f.filter).collect::<Vec<_>>(),
            vec![&Filter::Tag("work".into()), &Filter::IsAnswered(true)]
        );
        assert!(p.terms.is_empty());
    }

    #[test]
    fn mm2_compile_shapes_and_placeholder_counts() {
        for q in [
            "bcc:a body:hello attachment:x.pdf flag:answered is:draft date:2026-03..2026-04",
            "within:7d size:>1mb folder:Work -body:spam -date:2026",
            "from:alice date:..2026-01-01 subject:\"a b\" free text",
        ] {
            let c = compile(&parse(q));
            // `match_expr` (free text) binds separately; where_sql/where_params must agree.
            let placeholders = c.where_sql.matches('?').count();
            assert_eq!(placeholders, c.where_params.len(), "{q}: {}", c.where_sql);
        }
        let c = compile(&parse("body:hello"));
        assert_eq!(c.where_params[0], Value::Text("body : (\"hello\")".into()));
        assert!(c.where_sql.contains("message_fts MATCH ?"));
        let c = compile(&parse("date:2026-03"));
        assert_eq!(c.where_sql, "m.sent_at >= ? AND m.sent_at < ?");
    }

    /// Deterministic fuzz: pseudo-random strings over an alphabet full of the
    /// grammar's own metacharacters must never panic the parser or compiler,
    /// and the compiled fragment must always bind exactly as many values as
    /// it has placeholders.
    #[test]
    fn mm2_parser_and_compiler_survive_hostile_input() {
        let alphabet: Vec<&str> = vec![
            "from:",
            "to:",
            "body:",
            "date:",
            "within:",
            "size:",
            "folder:",
            "flag:",
            "attachment:",
            "bcc:",
            "is:",
            "has:",
            "before:",
            "after:",
            "in:",
            "tag:",
            "-",
            "\"",
            "..",
            ">",
            "<",
            ">=",
            "<=",
            "2026",
            "2026-03-15",
            "99999999999999999999",
            "7d",
            "1mb",
            "%",
            "_",
            "\\",
            "'",
            ";",
            " ",
            "  ",
            "é",
            "日本",
            "\u{0}",
            "NEAR(",
            "*",
            "(",
            ")",
            "a",
            "0",
        ];
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..4000 {
            let n = (next() % 9) as usize + 1;
            let q: String = (0..n)
                .map(|_| alphabet[(next() as usize) % alphabet.len()])
                .collect();
            let c = compile(&parse(&q));
            assert_eq!(
                c.where_sql.matches('?').count(),
                c.where_params.len(),
                "placeholder/bind mismatch for {q:?}: {}",
                c.where_sql
            );
        }
    }

    #[test]
    fn mm2_path_qualifier_is_exact() {
        assert_eq!(
            one("path:Clients/Projects").0,
            Filter::Path("Clients/Projects".into())
        );
        let c = compile(&parse("path:Projects"));
        assert_eq!(
            c.where_sql,
            "m.mailbox_id IN (SELECT id FROM mailboxes WHERE path = ?)"
        );
        assert_eq!(c.where_params, vec![Value::Text("Projects".into())]);
    }

    #[test]
    fn mm2_negated_nullable_filters_keep_rows_without_the_column() {
        for q in ["-date:2026", "-within:7d", "-size:>1mb"] {
            let c = compile(&parse(q));
            assert!(
                c.where_sql.starts_with("NOT COALESCE(("),
                "{q}: {}",
                c.where_sql
            );
        }
        // #3376: -before:/-after:/-subject: keep NULL rows too
        for q in ["-before:2026-01-01", "-after:2026-01-01", "-subject:foo"] {
            let c = compile(&parse(q));
            assert!(
                c.where_sql.starts_with("NOT COALESCE(("),
                "{q}: {}",
                c.where_sql
            );
        }
    }

    #[test]
    fn mm2_equals_prefix_means_exact_address() {
        let c = compile(&parse("from:=ann@x.test"));
        assert!(
            c.where_sql.contains("LOWER(ma.address) = LOWER(?)"),
            "{}",
            c.where_sql
        );
        assert_eq!(
            c.where_params,
            vec![Value::Text("from".into()), Value::Text("ann@x.test".into())]
        );
        // a lone "=" or plain value keeps the contains behaviour
        assert!(compile(&parse("from:=")).where_sql.contains("LIKE"));
        assert!(compile(&parse("from:ann")).where_sql.contains("LIKE"));
    }

    #[test]
    fn mm2_review_escaped_quote_and_month_length() {
        // \" inside a quoted span is a literal quote
        let (f, _) = one(r#"subject:"Re: \"Budget\" v2""#);
        assert_eq!(f, Filter::Subject(r#"Re: "Budget" v2"#.into()));
        // \\ is a literal backslash; other backslashes stay literal
        let (f, _) = one(r#"subject:"a\\b\c""#);
        assert_eq!(f, Filter::Subject(r"a\b\c".into()));
        // impossible calendar days are plain terms, not a rolled-over date
        for q in [
            "date:2026-02-31",
            "date:2026-04-31",
            "date:2027-02-29",
            "before:2026-02-30",
            "after:2026-06-31",
            "date:2026-02-28..2026-02-30",
        ] {
            assert!(
                !parse(q).terms.is_empty() && parse(q).filters.is_empty(),
                "{q} must not parse as a filter"
            );
        }
        // leap day is fine
        assert!(matches!(one("date:2028-02-29").0, Filter::DateRange(..)));
        assert!(!parse("date:2026-02-28").filters.is_empty());
    }

    // -- S42: tag: and is:muted ------------------------------------------

    #[test]
    fn tag_and_is_muted_are_recognised() {
        let q = parse("tag:Work is:muted");
        assert_eq!(
            q.filters,
            vec![
                ParsedFilter {
                    filter: Filter::Tag("Work".into()),
                    negated: false,
                },
                ParsedFilter {
                    filter: Filter::IsMuted,
                    negated: false,
                },
            ]
        );
        assert!(q.terms.is_empty());
    }

    #[test]
    fn tag_accepts_a_quoted_multi_word_value() {
        let q = parse(r#"tag:"To do""#);
        assert_eq!(
            q.filters,
            vec![ParsedFilter {
                filter: Filter::Tag("To do".into()),
                negated: false,
            }]
        );
    }

    #[test]
    fn tag_negation_forms_and_case_insensitive_qualifier_name() {
        let q = parse(r#"-tag:Work -tag:"To do" TAG:Home"#);
        assert_eq!(
            q.filters,
            vec![
                ParsedFilter {
                    filter: Filter::Tag("Work".into()),
                    negated: true,
                },
                ParsedFilter {
                    filter: Filter::Tag("To do".into()),
                    negated: true,
                },
                ParsedFilter {
                    filter: Filter::Tag("Home".into()),
                    negated: false,
                },
            ]
        );
        assert!(q.terms.is_empty());
    }

    #[test]
    fn empty_tag_value_degrades_to_a_free_text_term_without_panicking() {
        let q = parse("tag:");
        assert!(q.filters.is_empty());
        assert_eq!(terms(&q), vec![("tag:", false)]);
    }

    #[test]
    fn tag_compiles_to_a_keyword_or_display_name_match() {
        let (clause, params) = compile_filter(&Filter::Tag("Work".into()));
        assert!(clause.contains("message_keywords"));
        assert!(clause.contains("tag_definitions"));
        assert_eq!(
            params,
            vec![Value::Text("work".into()), Value::Text("Work".into())]
        );
    }

    #[test]
    fn is_muted_compiles_to_a_thread_lookup() {
        let (clause, params) = compile_filter(&Filter::IsMuted);
        assert!(clause.contains("threads"));
        assert!(params.is_empty());
    }

    #[test]
    fn is_vip_parses_and_compiles_to_a_vip_lookup() {
        assert_eq!(parse("is:vip").filters[0].filter, Filter::IsVip);
        let (clause, params) = compile_filter(&Filter::IsVip);
        assert!(clause.contains("vip_senders"));
        assert!(params.is_empty());
        let c = compile(&parse("is:vip is:unread"));
        assert!(c.where_sql.contains("vip_senders"));
    }

    #[test]
    fn quoted_phrase_compiles_to_one_fts_phrase() {
        let c = compile(&parse(r#""quick brown fox""#));
        assert_eq!(c.match_expr.as_deref(), Some("\"quick brown fox\""));
        assert_eq!(build_fts_phrase("quick, brown fox"), "\"quick brown fox\"");
        assert_eq!(build_fts_phrase("---"), "");
    }

    #[test]
    fn nfd_query_is_one_word() {
        assert_eq!(build_fts_query("ca\u{300}lcul"), "\"ca\u{300}lcul\"");
        assert_eq!(build_fts_phrase("ca\u{300}lcul"), "\"ca\u{300}lcul\"");
    }

    #[test]
    fn quoted_phrase_starting_with_operator_word_is_a_phrase() {
        for q in ["\"From: Amazon\"", "\"to: do list\"", "\"in: the news\""] {
            let p = parse(q);
            assert!(p.filters.is_empty(), "{q}: {:?}", p.filters);
            assert_eq!(p.terms.len(), 1, "{q}");
        }
        // an operator with a quoted value is still a filter
        let p = parse("subject:\"a b\"");
        assert_eq!(p.filters.len(), 1);
        assert!(p.terms.is_empty());
    }

    #[test]
    fn fold_text_drops_case_and_accents() {
        assert_eq!(fold_text("REUNIÓ"), "reunio");
        assert_eq!(fold_text("Nu\u{301}ria"), "nuria");
        assert_eq!(fold_text("Muñoz"), "munoz");
    }
}
