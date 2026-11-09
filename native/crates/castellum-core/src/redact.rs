// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//! Redaction-safe error classification for durable diagnostics.
//!
//! Classification is by variant discriminant ONLY: the inner message of every
//! error variant is intentionally never read, so server-echoed text,
//! credentials that leaked into a formatted error, or subject lines in a
//! parse error can never reach the diagnostics tables or the support bundle.
//! Every value this module produces is a compile-time constant.
//!
//! The support bundle re-validates persisted values against these known sets
//! on read (defence-in-depth against legacy or hand-edited rows).

use crate::CoreError;
use castellum_providers::{ProviderError, SessionAbandonReason};

/// Stable error-kind tags persisted in `*_sync_state.last_error_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Auth,
    /// SMTP AUTH is switched off for the mailbox or organisation (MS1). Its
    /// tag deliberately does not start with `auth:`: the app keys its
    /// forced-OAuth-refresh retry on that exact prefix, and a fresh token
    /// cannot change a server-side setting.
    AuthSmtpDisabled,
    Tls,
    Network,
    Protocol,
    Db,
    Unsupported,
    /// The SMTP server permanently refused the message (5xx, not auth).
    Rejected,
    /// The SMTP server answered with a transient 4xx (greylisting, full
    /// spool, rate limit). The server is reachable; the send is retried.
    Deferred,
    Internal,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Auth => "auth",
            ErrorKind::AuthSmtpDisabled => "auth_smtp_disabled",
            ErrorKind::Tls => "tls",
            ErrorKind::Network => "network",
            ErrorKind::Protocol => "protocol",
            ErrorKind::Db => "db",
            ErrorKind::Unsupported => "unsupported",
            ErrorKind::Rejected => "rejected",
            ErrorKind::Deferred => "deferred",
            ErrorKind::Internal => "internal",
        }
    }
}

/// Every kind tag this module can emit. Bundle generation treats anything
/// else found in the database as untrusted and replaces it.
pub const KNOWN_ERROR_KINDS: &[&str] = &[
    "auth",
    "auth_smtp_disabled",
    "tls",
    "network",
    "protocol",
    "db",
    "unsupported",
    "rejected",
    "deferred",
    "internal",
];

/// Every redacted summary phrase this module can emit.
pub const KNOWN_SUMMARIES: &[&str] = &[
    "authentication failed",
    "SMTP sending is switched off for this mailbox",
    "TLS handshake failed",
    "the server refused the message",
    "the server deferred the message",
    "network connection failed",
    "IMAP protocol error",
    "capability not supported by provider",
    "database error",
    "sync error",
    "invalid argument",
    "I/O error",
];

/// Sync phases persisted in `*_sync_state.last_error_phase`.
pub mod phase {
    pub const CONNECT: &str = "connect";
    pub const SYNC: &str = "sync";
    pub const SELECT: &str = "select";
    pub const FETCH: &str = "fetch";
    pub const HYDRATE: &str = "hydrate";
    /// A supervised IMAP IDLE connection (S31) — distinguishes an
    /// idle-loop connect/IDLE failure from an ordinary sync's own
    /// connect/sync phases in `account_sync_state`'s diagnostics.
    pub const IDLE: &str = "idle";
}

/// Every phase tag this module can emit.
pub const KNOWN_PHASES: &[&str] = &[
    phase::CONNECT,
    phase::SYNC,
    phase::SELECT,
    phase::FETCH,
    phase::HYDRATE,
    phase::IDLE,
];

/// Map a `ProviderError` to its safe (kind, fixed summary) pair. The inner
/// `String` of each variant is intentionally ignored.
pub fn redact_provider_error(e: &ProviderError) -> (ErrorKind, &'static str) {
    match e {
        ProviderError::Auth(_) => (ErrorKind::Auth, "authentication failed"),
        ProviderError::AuthSmtpDisabled(_) => (
            ErrorKind::AuthSmtpDisabled,
            "SMTP sending is switched off for this mailbox",
        ),
        ProviderError::Tls(_) => (ErrorKind::Tls, "TLS handshake failed"),
        ProviderError::Network(_) => (ErrorKind::Network, "network connection failed"),
        ProviderError::Protocol(_) => (ErrorKind::Protocol, "IMAP protocol error"),
        ProviderError::Unsupported(_) => (
            ErrorKind::Unsupported,
            "capability not supported by provider",
        ),
        ProviderError::Rejected(_) => (ErrorKind::Rejected, "the server refused the message"),
        ProviderError::Deferred(_) => (ErrorKind::Deferred, "the server deferred the message"),
        ProviderError::MaybeDelivered(_) => (
            ErrorKind::Network,
            "the connection dropped after the message was submitted",
        ),
    }
}

/// Map why `MailProvider::session_abandoned()` is true to its safe (kind,
/// fixed summary) pair. Only a session left poisoned by an
/// undecodable response, with nothing having tried to replace it, still
/// reads as "protocol" — a closed connection, or a recovery attempt that
/// itself could not get a working session back, reads as "network" like
/// any other dropped-connection failure. `None` falls back to "protocol"
/// defensively; callers only reach this once `session_abandoned()` is
/// already known `true`, so `None` should not occur in practice.
pub fn redact_session_abandon_reason(
    reason: Option<SessionAbandonReason>,
) -> (ErrorKind, &'static str) {
    match reason {
        Some(SessionAbandonReason::Closed) | Some(SessionAbandonReason::BudgetExhausted) => {
            (ErrorKind::Network, "network connection failed")
        }
        Some(SessionAbandonReason::Poisoned) | None => (ErrorKind::Protocol, "IMAP protocol error"),
    }
}

/// Map a `CoreError` to its safe (kind, fixed summary) pair. The inner
/// `String` of each variant is intentionally ignored.
pub fn redact_core_error(e: &CoreError) -> (ErrorKind, &'static str) {
    match e {
        CoreError::InvalidArgument => (ErrorKind::Internal, "invalid argument"),
        CoreError::Io => (ErrorKind::Internal, "I/O error"),
        CoreError::Sync(_) => (ErrorKind::Protocol, "sync error"),
        CoreError::Db(_) => (ErrorKind::Db, "database error"),
        // Same closed tag/summary as InvalidArgument — a stale snapshot or a
        // legacy payload is a precondition failure, not a new category.
        CoreError::NonRetryable(_) => (ErrorKind::Internal, "invalid argument"),
        CoreError::SchemaTooNew(_) => (ErrorKind::Db, "profile schema too new"),
        CoreError::MigrationFailed(_) => (ErrorKind::Db, "database migration failed"),
    }
}

/// Closed allowlist of exportable IMAP CAPABILITY atoms, in canonical
/// spelling. CAPABILITY tokens are server-controlled text: a charset filter
/// still lets a hostile server smuggle identifiers built from permitted
/// characters (`X-SOURCE-IP-203.0.113.4`, `ACCOUNT-ALICE-SECRET`) into the
/// persisted capture and the exported support bundle. A token therefore
/// survives export only as the canonical spelling of an explicitly known
/// public capability name or SASL mechanism; everything else collapses into
/// an `OTHER(n)` count that carries no server bytes.
const KNOWN_CAPABILITY_ATOMS: &[&str] = &[
    "IMAP4rev1",
    "IMAP4rev2",
    "STARTTLS",
    "LOGINDISABLED",
    "ID",
    "IDLE",
    "UIDPLUS",
    "MOVE",
    "NAMESPACE",
    "ENABLE",
    "CONDSTORE",
    "QRESYNC",
    "LITERAL+",
    "LITERAL-",
    "SASL-IR",
    "UNSELECT",
    "CHILDREN",
    "BINARY",
    "CATENATE",
    "ESEARCH",
    "SEARCHRES",
    "WITHIN",
    "SORT",
    "SORT=DISPLAY",
    "THREAD=ORDEREDSUBJECT",
    "THREAD=REFERENCES",
    "THREAD=REFS",
    "MULTIAPPEND",
    "COMPRESS=DEFLATE",
    "QUOTA",
    "ACL",
    "SPECIAL-USE",
    "CREATE-SPECIAL-USE",
    "LIST-EXTENDED",
    "LIST-STATUS",
    "STATUS=SIZE",
    "OBJECTID",
    "SAVEDATE",
    "PREVIEW",
    "UTF8=ACCEPT",
    "UTF8=ONLY",
    "METADATA",
    "METADATA-SERVER",
    "NOTIFY",
    "LOGIN-REFERRALS",
    "MAILBOX-REFERRALS",
    "XLIST",
    "X-GM-EXT-1",
    "AUTH=PLAIN",
    "AUTH=LOGIN",
    "AUTH=XOAUTH2",
    "AUTH=OAUTHBEARER",
    "AUTH=CRAM-MD5",
    "AUTH=DIGEST-MD5",
    "AUTH=SCRAM-SHA-1",
    "AUTH=SCRAM-SHA-256",
    "AUTH=SCRAM-SHA-512",
    "AUTH=GSSAPI",
    "AUTH=NTLM",
    "AUTH=ANONYMOUS",
    "AUTH=EXTERNAL",
];

/// Known parameterised capabilities whose server-chosen value is dropped and
/// only the base name (a canonical category) is exported. `APPENDLIMIT=n`
/// becomes `APPENDLIMIT`; the numeric value is server data and stays out.
const KNOWN_CAPABILITY_PREFIXES: &[&str] = &["APPENDLIMIT"];

/// Cap on exported atoms; a hostile server cannot inflate the capture.
const MAX_CAPABILITY_TOKENS: usize = 64;

/// Cap on the unrecognised-token count so the count itself stays small.
const MAX_OTHER_COUNT: u32 = 999;

fn canonical_capability(token: &str) -> Option<&'static str> {
    let atom = KNOWN_CAPABILITY_ATOMS
        .iter()
        .find(|k| k.eq_ignore_ascii_case(token));
    if let Some(k) = atom {
        return Some(k);
    }
    let base = token.split('=').next().unwrap_or(token);
    KNOWN_CAPABILITY_PREFIXES
        .iter()
        .find(|k| k.eq_ignore_ascii_case(base))
        .copied()
}

fn other_marker(count: u32) -> String {
    format!("OTHER({})", count.min(MAX_OTHER_COUNT))
}

/// Parse a previously exported `OTHER(n)` marker. The count is re-emitted
/// from the parsed integer, so even a marker-shaped hostile row cannot pass
/// server bytes through.
fn parse_other_marker(token: &str) -> Option<u32> {
    let inner = token.strip_prefix("OTHER(")?.strip_suffix(')')?;
    if inner.is_empty() || inner.len() > 3 || !inner.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    inner.parse().ok()
}

/// Reduce raw server CAPABILITY tokens to the export-safe representation:
/// canonical spellings of allowlisted atoms (deduplicated, count-capped) plus
/// one `OTHER(n)` marker summarising unrecognised extensions. No server byte
/// ever passes through — every output string is a compile-time constant or a
/// formatted integer. Applied at capture AND re-applied when reading
/// persisted rows (`sanitize_capability_tokens` is idempotent: canonical
/// atoms map to themselves and `OTHER(n)` markers merge by parsed count).
pub fn sanitize_capability_tokens(tokens: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut other: u32 = 0;
    for token in tokens {
        if let Some(canon) = canonical_capability(token) {
            if !out.iter().any(|t| t == canon) && out.len() < MAX_CAPABILITY_TOKENS {
                out.push(canon.to_string());
            }
        } else if let Some(n) = parse_other_marker(token) {
            other = other.saturating_add(n);
        } else {
            other = other.saturating_add(1);
        }
    }
    if other > 0 {
        out.push(other_marker(other));
    }
    out
}

// ---------------------------------------------------------------------------
// Free-text redaction for diagnostics export (crash reports, log excerpts)
// ---------------------------------------------------------------------------
//
// Everything above this line classifies KNOWN shapes (capability atoms, error
// kinds) against closed allowlists. Crash reports and unified-log lines are
// OS/Apple-shaped free text with no closed vocabulary to allowlist against, so
// `redact_diagnostic_text` instead scrubs the two patterns that matter here —
// an email address, and a `key=value` / `key: value` / `Bearer <token>`
// credential — replacing the match with a fixed marker. This is
// defence-in-depth, not a guarantee: Castellum itself never logs mail content
// or credentials (nothing in this codebase calls `os_log`/`NSLog`/`print`
// with message data), so in the common case there is nothing here to remove.
// Deliberately regex-free, matching this file's existing hand-rolled style.

/// Case-insensitive whole-word markers that, once found in a line of free
/// text, cause everything after their separator (`=`, `:`, or the plain space
/// before a `Bearer` token) to be replaced by a single marker. Coarse on
/// purpose — losing the rest of a short crash/log line is a safe failure,
/// leaking a credential is not.
const SECRET_LIKE_KEYS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "pass",
    "token",
    "secret",
    "apikey",
    "api_key",
    "api-key",
    "authorization",
    "bearer",
];

const REDACTED_ADDRESS: &str = "[redacted-address]";
const REDACTED_SECRET: &str = "[redacted]";

/// Best-effort redaction of OS-generated free text (crash reports, unified-log
/// excerpts) for the diagnostics bundle. Operates line by line so a redaction
/// on one line never eats unrelated content on the next. Never panics: every
/// byte index used for slicing comes from `char_indices`/`str::find`, which
/// only ever return char-boundary-aligned offsets, so arbitrary (including
/// non-UTF8-adjacent-looking) input cannot trigger a slicing panic.
pub fn redact_diagnostic_text(input: &str) -> String {
    input.split_inclusive('\n').map(redact_line).collect()
}

fn redact_line(line: &str) -> String {
    let (body, newline) = match line.strip_suffix('\n') {
        Some(b) => (b, "\n"),
        None => (line, ""),
    };
    let addresses_redacted = redact_email_addresses(body);
    format!(
        "{}{}",
        redact_secret_like_suffix(&addresses_redacted),
        newline
    )
}

/// Non-ASCII characters (accented letters, IDN labels, combining marks) are
/// part of an address: an ASCII-only class stopped at `é` and left
/// `josé@gmail.com` unredacted (#3374). Over-redacting is the safe direction.
fn is_non_ascii_name_char(c: char) -> bool {
    !c.is_ascii() && !c.is_whitespace() && !c.is_control()
}

/// RFC 5322 `atext` plus the dot. The earlier class had only `._%+-`, so
/// `o'brien@x.com` kept its `o'` and `user=tag@x.com` kept `user=` (#3472).
/// Including `=` and `/` also eats a `key=` or path prefix glued to the
/// address; losing that is the safe direction.
fn is_local_part_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || is_non_ascii_name_char(c)
        || matches!(
            c,
            '.' | '!'
                | '#'
                | '$'
                | '%'
                | '&'
                | '\''
                | '*'
                | '+'
                | '-'
                | '/'
                | '='
                | '?'
                | '^'
                | '_'
                | '`'
                | '{'
                | '|'
                | '}'
                | '~'
        )
}

fn is_domain_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || is_non_ascii_name_char(c) || matches!(c, '.' | '-' | '_')
}

/// Length in chars of an "at" separator starting at `idx`, or 0: a plain `@`,
/// the full-width `＠` (U+FF20) and small `﹫` (U+FE6B) forms, or the
/// percent-encoded `%40` a URL or a log formatter leaves behind (#3472).
fn at_sign_len(chars: &[(usize, char)], idx: usize) -> usize {
    match chars.get(idx).map(|&(_, c)| c) {
        Some('@' | '\u{FF20}' | '\u{FE6B}') => 1,
        Some('%')
            if chars.get(idx + 1).map(|&(_, c)| c) == Some('4')
                && chars.get(idx + 2).map(|&(_, c)| c) == Some('0') =>
        {
            3
        }
        _ => 0,
    }
}

/// Longest run scanned to the left of an "at" for the local part. Far above
/// the 64-octet limit of a real local part; it only bounds the work a hostile
/// line of thousands of separators can cause.
const MAX_LOCAL_SCAN: usize = 1024;

/// Replace every address-shaped span with a fixed marker.
///
/// Deliberately loose (#3472): anything of the form `local SEP domain` goes,
/// where the domain may be dotless (`alice@localhost`), a bracketed literal
/// (`alice@[192.168.0.1]`), or followed by sentence punctuation
/// (`alice@example.com.` used to fail the "no empty label" check and survive
/// whole), and the local part may be a quoted string (`"john doe"@x.com`).
/// False positives cost a few characters of a log line; a miss leaks an
/// address into a support bundle.
fn redact_email_addresses(input: &str) -> String {
    let chars: Vec<(usize, char)> = input.char_indices().collect();
    let n = chars.len();
    let mut out = String::with_capacity(input.len());
    let mut last_copied = 0usize;
    // Char index matching `last_copied`: nothing left of it is examined again,
    // so a second address glued to a redacted one (`a@b.com_c@d.com`) can never
    // produce a span that starts before the previous one ended.
    let mut floor = 0usize;
    let mut idx = 0usize;
    while idx < n {
        let at = at_sign_len(&chars, idx);
        if at == 0 {
            idx += 1;
            continue;
        }

        // Local part, scanning left from the separator.
        let scan_floor = floor.max(idx.saturating_sub(MAX_LOCAL_SCAN));
        let mut left = idx;
        if left > floor && chars[left - 1].1 == '"' {
            // Quoted local part: back to the opening quote. With no opening
            // quote in reach, fail closed and take everything back to the
            // floor rather than leave the quoted text behind.
            let mut q = left - 1;
            let mut open = None;
            while q > scan_floor {
                q -= 1;
                if chars[q].1 == '"' && (q == 0 || chars[q - 1].1 != '\\') {
                    open = Some(q);
                    break;
                }
            }
            left = open.unwrap_or(floor);
        } else {
            while left > scan_floor && is_local_part_char(chars[left - 1].1) {
                left -= 1;
            }
        }

        // Domain, scanning right; `a@b@c.example` is taken as one span.
        let domain_start = idx + at;
        let mut right = domain_start;
        loop {
            if right < n && chars[right].1 == '[' {
                right += 1;
                while right < n
                    && (chars[right].1.is_ascii_alphanumeric()
                        || matches!(chars[right].1, '.' | ':' | '-'))
                {
                    right += 1;
                }
                if right < n && chars[right].1 == ']' {
                    right += 1;
                }
            } else {
                while right < n && is_domain_char(chars[right].1) {
                    right += 1;
                }
            }
            let more = at_sign_len(&chars, right);
            let next = right + more;
            if more > 0 && next < n && (is_domain_char(chars[next].1) || chars[next].1 == '[') {
                right = next;
            } else {
                break;
            }
        }
        // Sentence punctuation after the address is not part of the domain.
        while right > domain_start && matches!(chars[right - 1].1, '.' | '-' | '_') {
            right -= 1;
        }

        if left < idx && right > domain_start {
            let start_byte = chars[left].0;
            let end_byte = if right < n {
                chars[right].0
            } else {
                input.len()
            };
            out.push_str(&input[last_copied..start_byte]);
            out.push_str(REDACTED_ADDRESS);
            last_copied = end_byte;
            floor = right;
            idx = right;
        } else {
            idx += at;
        }
    }
    out.push_str(&input[last_copied..]);
    out
}

/// Byte offset of `needle` in `haystack` (both expected lowercase already) as
/// a standalone word — not a substring of a longer identifier — or `None`.
fn find_word(haystack: &str, needle: &str) -> Option<usize> {
    let mut start = 0;
    while let Some(rel) = haystack[start..].find(needle) {
        let pos = start + rel;
        let before_ok = pos == 0 || !haystack.as_bytes()[pos - 1].is_ascii_alphanumeric();
        let end = pos + needle.len();
        let after_ok = end == haystack.len() || !haystack.as_bytes()[end].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return Some(pos);
        }
        start = pos + 1;
    }
    None
}

/// If a secret-like key appears as a whole word in `line`, drop everything
/// from right after its separator to the end of the line and replace it with
/// one marker. `line` and its lowercased copy have identical byte offsets
/// (`to_ascii_lowercase` only touches ASCII A-Z), so offsets found in the
/// lowercased copy are safe to slice `line` with directly.
fn redact_secret_like_suffix(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let earliest_key = SECRET_LIKE_KEYS
        .iter()
        .filter_map(|key| find_word(&lower, key).map(|pos| (pos, key.len())))
        .min_by_key(|&(pos, _)| pos);
    let Some((key_start, key_len)) = earliest_key else {
        return line.to_string();
    };
    let after_key = &line[key_start + key_len..];
    let sep_len = after_key
        .find(|c: char| !c.is_whitespace() && c != '=' && c != ':')
        .unwrap_or(after_key.len());
    if sep_len == after_key.len() {
        // The key has nothing after it (e.g. "...the password field" with no
        // value) — nothing to redact.
        return line.to_string();
    }
    format!(
        "{}{}",
        &line[..key_start + key_len + sep_len],
        REDACTED_SECRET
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Sentinels that must never survive redaction.
    const PASS_SENTINEL: &str = "hunter2-super-secret-SENTINEL";
    const SERVER_SENTINEL: &str = "NO [AUTHENTICATIONFAILED] user@host SENTINEL";
    const SUBJECT_SENTINEL: &str = "SECRET-PROJECT-BRIEFING-SENTINEL";

    #[test]
    fn provider_error_redaction_never_passes_inner_text_through() {
        let cases = [
            ProviderError::Auth(format!("{SERVER_SENTINEL} password={PASS_SENTINEL}")),
            ProviderError::Tls(SERVER_SENTINEL.into()),
            ProviderError::Network(SERVER_SENTINEL.into()),
            ProviderError::Protocol(format!("BAD {SUBJECT_SENTINEL}")),
            ProviderError::Unsupported(SUBJECT_SENTINEL.into()),
            ProviderError::Rejected(SERVER_SENTINEL.into()),
            ProviderError::Deferred(format!("451 4.7.1 {SERVER_SENTINEL}")),
        ];
        for e in &cases {
            let (kind, summary) = redact_provider_error(e);
            let combined = format!("{} {}", kind.as_str(), summary);
            for s in [PASS_SENTINEL, SERVER_SENTINEL, SUBJECT_SENTINEL] {
                assert!(!combined.contains(s), "sentinel leaked: {combined}");
            }
            assert!(KNOWN_ERROR_KINDS.contains(&kind.as_str()));
            assert!(KNOWN_SUMMARIES.contains(&summary));
        }
    }

    #[test]
    fn core_error_redaction_never_passes_inner_text_through() {
        let cases = [
            CoreError::InvalidArgument,
            CoreError::Io,
            CoreError::Sync(format!("connect to {SERVER_SENTINEL}: {PASS_SENTINEL}")),
            CoreError::Db(SUBJECT_SENTINEL.into()),
            CoreError::NonRetryable(SUBJECT_SENTINEL.into()),
        ];
        for e in &cases {
            let (kind, summary) = redact_core_error(e);
            let combined = format!("{} {}", kind.as_str(), summary);
            for s in [PASS_SENTINEL, SERVER_SENTINEL, SUBJECT_SENTINEL] {
                assert!(!combined.contains(s), "sentinel leaked: {combined}");
            }
            assert!(KNOWN_ERROR_KINDS.contains(&kind.as_str()));
            assert!(KNOWN_SUMMARIES.contains(&summary));
        }
    }

    #[test]
    fn phases_are_all_known() {
        for p in [
            phase::CONNECT,
            phase::SYNC,
            phase::SELECT,
            phase::FETCH,
            phase::HYDRATE,
        ] {
            assert!(KNOWN_PHASES.contains(&p));
        }
    }

    #[test]
    fn capability_sanitizer_exports_only_allowlisted_canonical_atoms() {
        let tokens: Vec<String> = [
            "IMAP4rev1",
            "uidplus", // canonicalised case-insensitively
            "MOVE",
            "IDLE",
            "NAMESPACE",
            "CONDSTORE",
            "QRESYNC",
            "AUTH=PLAIN",
            "auth=xoauth2",
            "X-GM-EXT-1",
            "LITERAL+",
            "APPENDLIMIT=35651584", // value dropped, category kept
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let out = sanitize_capability_tokens(&tokens);
        for canon in [
            "IMAP4rev1",
            "UIDPLUS",
            "MOVE",
            "IDLE",
            "NAMESPACE",
            "CONDSTORE",
            "QRESYNC",
            "AUTH=PLAIN",
            "AUTH=XOAUTH2",
            "X-GM-EXT-1",
            "LITERAL+",
            "APPENDLIMIT",
        ] {
            assert!(out.contains(&canon.to_string()), "missing {canon}: {out:?}");
        }
        assert!(!out.iter().any(|t| t.contains("35651584")));
    }

    #[test]
    fn capability_sanitizer_never_echoes_unknown_tokens() {
        // Hostile atoms built ONLY from the previously permitted charset:
        // identifiers, IP-shaped values, emails-in-disguise, encoded blobs.
        let hostile = [
            "X-SOURCE-IP-203.0.113.4",
            "ACCOUNT-ALICE-SECRET",
            "USER=alice.example.com",
            "SID-aGVsbG8td29ybGQ=",
            "TRACE-0xDEADBEEF",
            "AUTH=EVIL-MECH-alice",
        ];
        let tokens: Vec<String> = hostile
            .iter()
            .map(|s| s.to_string())
            .chain(std::iter::once("IMAP4rev1".to_string()))
            .collect();
        let out = sanitize_capability_tokens(&tokens);
        let joined = out.join(" ");
        for h in hostile {
            assert!(!joined.contains(h), "hostile token leaked: {joined}");
        }
        for fragment in ["203.0.113.4", "ALICE", "alice", "aGVsbG8", "DEADBEEF"] {
            assert!(!joined.contains(fragment), "fragment leaked: {joined}");
        }
        assert_eq!(
            out,
            vec!["IMAP4rev1".to_string(), "OTHER(6)".to_string()],
            "unknowns must collapse into a pure count"
        );
    }

    #[test]
    fn capability_sanitizer_is_idempotent_and_reparses_other_markers() {
        let first = sanitize_capability_tokens(&[
            "IMAP4rev1".to_string(),
            "X-MYSTERY-1".to_string(),
            "X-MYSTERY-2".to_string(),
        ]);
        assert_eq!(first, vec!["IMAP4rev1", "OTHER(2)"]);
        // Re-applying on read keeps the same representation.
        assert_eq!(sanitize_capability_tokens(&first), first);
        // A marker-SHAPED hostile row cannot pass bytes through: it either
        // parses as a pure integer (re-emitted from the parse) or counts as
        // one unknown.
        let tampered = sanitize_capability_tokens(&[
            "OTHER(evil-payload)".to_string(),
            "OTHER(12)".to_string(),
        ]);
        assert_eq!(tampered, vec!["OTHER(13)"]);
        let flood = sanitize_capability_tokens(&["OTHER(99999999)".to_string()]);
        assert_eq!(flood, vec!["OTHER(1)"], "oversized counts are untrusted");
    }

    #[test]
    fn capability_sanitizer_caps_atom_count_and_other_count() {
        // Even 500 distinct ALLOWLISTED spellings cannot exceed the cap
        // (dedup makes this moot in practice; the cap is belt-and-braces),
        // and unknown floods clamp to a bounded count.
        let flood: Vec<String> = (0..5000).map(|i| format!("CAP{i}")).collect();
        let out = sanitize_capability_tokens(&flood);
        assert_eq!(out, vec!["OTHER(999)"]);
    }

    // ── redact_diagnostic_text: free-text redaction for crash reports/logs ──

    #[test]
    fn redacts_an_address_embedded_in_a_subject_like_line() {
        let input = r#"Last action: opened message "Please review, alice@example.test" in INBOX"#;
        let out = redact_diagnostic_text(input);
        assert!(!out.contains("alice@example.test"));
        assert!(out.contains(REDACTED_ADDRESS));
    }

    #[test]
    fn redacts_non_ascii_addresses_entirely() {
        // #3374: the ASCII-only scan left these in the exported log excerpt.
        for (addr, parts) in [
            ("jos\u{e9}@gmail.com", vec!["jos", "gmail"]),
            ("marc@b\u{fc}cher.de", vec!["marc", "cher.de"]),
            ("ramon.n\u{fa}\u{f1}ez@x.com", vec!["ramon", "ez@"]),
            ("jose\u{301}@gmail.com", vec!["jose", "gmail"]),
        ] {
            let out = redact_diagnostic_text(&format!("sync ok account_id={addr} done"));
            for p in parts {
                assert!(!out.contains(p), "{addr}: {p} survived in {out}");
            }
            assert!(out.contains(REDACTED_ADDRESS), "{out}");
            assert!(out.contains("done"), "{out}");
        }
    }

    /// #3472 item 3: every shape the security re-review listed as leaking.
    #[test]
    fn redacts_the_address_shapes_the_security_review_listed() {
        let cases: &[(&str, &[&str])] = &[
            // Followed by a full stop: used to survive WHOLE.
            ("Login failed for alice@example.com.", &["alice", "example"]),
            ("see bob@example.co.uk, then retry", &["bob", "example"]),
            ("user carol@example.com- dropped", &["carol", "example"]),
            // Quoted local part.
            (
                "rcpt \"john doe\"@example.com rejected",
                &["john", "doe", "example"],
            ),
            (
                "rcpt \"a\\\"b c\"@example.com rejected",
                &["b c", "example"],
            ),
            // Apostrophe and "=" inside or in front of the local part.
            ("from o'brien@example.com ok", &["brien", "example"]),
            ("from user=tag@example.com ok", &["user", "tag", "example"]),
            ("from =alice@example.com ok", &["alice", "example"]),
            (
                "from a!b#c$d&e*f/g?h^i`j{k|l}m~n@example.com ok",
                &["a!b", "m~n", "example"],
            ),
            // Dotless and bracketed domains.
            ("from alice@localhost ok", &["alice", "localhost"]),
            ("from alice@[192.168.0.1] ok", &["alice", "192.168"]),
            (
                "from alice@[IPv6:2001:db8::1] ok",
                &["alice", "2001", "db8"],
            ),
            // Percent-encoded and full-width separators.
            ("GET /u?m=alice%40example.com ok", &["alice", "example"]),
            ("from alice\u{FF20}example.com ok", &["alice", "example"]),
            ("from alice\u{FE6B}example.com ok", &["alice", "example"]),
            // Chained separators.
            (
                "from alice@relay@example.com ok",
                &["alice", "relay", "example"],
            ),
        ];
        for (input, fragments) in cases {
            let out = redact_diagnostic_text(input);
            for f in *fragments {
                assert!(!out.contains(f), "{input:?}: {f:?} survived in {out:?}");
            }
            assert!(out.contains(REDACTED_ADDRESS), "{input:?} -> {out:?}");
            assert_eq!(redact_diagnostic_text(&out), out, "idempotent: {input:?}");
        }
        // Text after the address on the same line survives.
        assert!(redact_diagnostic_text("for alice@example.com. Retrying").contains("Retrying"));
        // A quote with no opening partner fails closed (nothing before it stays).
        let out = redact_diagnostic_text("rcpt john doe\"@example.com");
        assert!(!out.contains("john") && !out.contains("example"), "{out}");
    }

    /// Two addresses glued by a local-part character: the second span used to
    /// start before the first one ended and panicked on the slice.
    #[test]
    fn adjacent_addresses_never_overlap_or_panic() {
        for input in [
            "a@b.com_c@d.com",
            "a@b.com=c@d.com",
            "x%40y%40z%40",
            "\"@\"@\"@",
            "a@[",
            "a@[]@[",
            "%40%40%40",
        ] {
            let out = redact_diagnostic_text(input);
            assert!(!out.contains("b.com") && !out.contains("d.com"), "{out}");
        }
        let long = format!("{}@example.com", "a".repeat(5000));
        assert!(!redact_diagnostic_text(&long).contains("example"));
    }

    #[test]
    fn redacts_an_address_embedded_in_an_error_string() {
        let input = "NO [AUTHENTICATIONFAILED] for user alice.secret@example.test SENTINEL";
        let out = redact_diagnostic_text(input);
        assert!(!out.contains("alice.secret@example.test"));
        assert!(
            out.contains("SENTINEL"),
            "unrelated text on the same line must survive: {out}"
        );
    }

    #[test]
    fn redacts_a_password_looking_key_value_token() {
        let input = "connect failed: password=SENTINEL_NOT_A_REAL_SECRET retrying";
        let out = redact_diagnostic_text(input);
        assert!(
            !out.contains("SENTINEL"),
            "everything after the key is dropped, not just the first word: {out}"
        );
        assert!(
            out.contains(REDACTED_SECRET),
            "no redaction marker left: {out}"
        );
    }

    #[test]
    fn redacts_a_bearer_token_after_an_authorization_header() {
        let input = "Authorization: Bearer abc123XYZ-SENTINEL";
        let out = redact_diagnostic_text(input);
        assert!(!out.contains("abc123XYZ"));
        assert!(!out.contains("SENTINEL"));
        assert!(out.starts_with("Authorization: "));
    }

    #[test]
    fn only_redacts_the_offending_line_leaving_others_intact() {
        let input = "line one is fine\npassword=SENTINEL_NOT_A_REAL_SECRET\nline three is fine\n";
        let out = redact_diagnostic_text(input);
        assert!(out.contains("line one is fine"));
        assert!(out.contains("line three is fine"));
        assert!(
            !out.contains("SENTINEL"),
            "the offending line must actually be redacted: {out}"
        );
        let middle = out.lines().nth(1).unwrap_or("");
        assert!(
            middle.contains(REDACTED_SECRET),
            "middle line must carry the redaction marker: {out}"
        );
    }

    #[test]
    fn leaves_ordinary_text_untouched() {
        let input = "Castellum 0.3.6 (1) launched on macOS 15.1 (24B83)";
        assert_eq!(redact_diagnostic_text(input), input);
    }

    #[test]
    fn is_idempotent_on_already_redacted_text() {
        let once = redact_diagnostic_text("password=SENTINEL_NOT_A_REAL_SECRET alice@example.test");
        let twice = redact_diagnostic_text(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn never_panics_on_arbitrary_bytes_reinterpreted_as_utf8() {
        // Crash reports are not guaranteed pure ASCII (embedded binary blobs,
        // truncated multi-byte sequences via lossy decoding). This must never
        // panic on a char-boundary slicing bug regardless of shape.
        let fixtures = [
            "@@@@@@@@@@",
            "a@",
            "@b.com",
            "password=",
            "bearer",
            "😀@example.com password=🔑-SENTINEL",
            "",
        ];
        for f in fixtures {
            let _ = redact_diagnostic_text(f);
        }
    }
}
