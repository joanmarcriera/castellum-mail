// Castellum - SPF/DKIM/DMARC trust signal extraction (S48).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Reads the Authentication-Results header a message already carries (RFC 8601)
// and reduces it to a compact, non-alarmist pass/fail/none per mechanism.
//
// THREAT MODEL: any sender can put ANY text in the message they compose,
// including a forged "Authentication-Results: mx.google.com; spf=pass
// dkim=pass dmarc=pass" header claiming full authentication. Castellum must
// never show that as a trusted badge. Two independent properties defend
// against this:
//
//   1. Every conforming mail hop PREPENDS its own trace headers to the TOP of
//      the header block rather than editing existing ones (RFC 5322 §3.6,
//      RFC 8601 §5). So the OUTERMOST (first, in document/header order)
//      Authentication-Results header was added by the LAST hop before the
//      message reached local storage — an attacker's own forged header,
//      present in the body they authored, can only ever end up BELOW it,
//      because the message must transit at least one real MTA (the
//      recipient's own) to reach the user's mailbox. We therefore only ever
//      look at `header_as(...).first()`.
//
//   2. Outermost alone is not sufficient: if the account's own provider adds
//      no Authentication-Results header at all (many self-hosted servers
//      don't — GreenMail, this repo's own IMAP test fixture, never does),
//      the attacker's forged header becomes the only — and therefore
//      "outermost" — one, and the account's IMAP host name is guessable
//      (it is usually in the recipient's MX or autoconfig record), so "the
//      authserv-id equals the IMAP host" proves nothing (#3472). A verdict is
//      therefore read ONLY when the (IMAP host, authserv-id) pair is in the
//      closed `KNOWN_PROVIDERS` table: providers known to stamp their own
//      header on every inbound message. Any other host reads `None`
//      ("no verification available") for every mechanism, never a false
//      `Pass`. See DECISIONS.md 2026-10-13. This is a heuristic, not a
//      cryptographic check — no signature is re-verified — so the UI treats
//      it as informational (see ReadingPaneView / TrustBadgeView) and the
//      details popover names the authserv-id it saw, or says it found none.
//
//   3. Fail closed on anything that cannot be parsed with certainty: a header
//      that ends inside a comment or a quoted string has no trustworthy
//      structure, so it yields no verdict at all (#3472).

use castellum_models::{TrustResult, TrustSignals};
use mail_parser::{HeaderForm, HeaderName, MessageParser};

/// Extract SPF/DKIM/DMARC trust signals from a raw RFC822 message, scoped to
/// `provider_host` (the account's configured IMAP host, e.g.
/// "imap.fastmail.com" — `ImapSyncConfig::host`). Never panics; a message
/// that fails to parse, or carries no header whose `authserv-id` matches the
/// account's own provider, returns every field `TrustResult::None`.
pub fn extract_trust_signals(raw: &[u8], provider_host: &str) -> TrustSignals {
    let Some(message) = MessageParser::default().parse(raw) else {
        return TrustSignals::default();
    };

    let outermost_auth_results = message
        .header_as(HeaderName::AuthenticationResults, HeaderForm::Raw)
        .into_iter()
        .find_map(|v| v.as_text().map(str::to_owned));

    match outermost_auth_results {
        Some(header) => from_authentication_results(&header, provider_host),
        // Some providers only emit the older, SPF-only trace header.
        None => from_received_spf(raw, provider_host),
    }
}

fn from_authentication_results(header: &str, provider_host: &str) -> TrustSignals {
    let Some((authserv_id, mechanisms)) = header.split_once(';') else {
        return TrustSignals::default();
    };
    let authserv_id = authserv_id.trim();
    // Name what we saw (for the details popover) but never a pass/fail.
    let unknown = || TrustSignals {
        authserv_id: (!authserv_id.is_empty()).then(|| authserv_id.to_owned()),
        ..TrustSignals::default()
    };
    if authserv_id.is_empty() || !authserv_matches(authserv_id, provider_host) {
        return unknown();
    }
    // Fail closed: an unterminated comment or quoted string means the header
    // cannot be parsed with certainty, so NO mechanism gets a verdict. Reading
    // the part before the unbalanced "(" would keep an early `spf=pass` while
    // dropping a later `dmarc=fail`, and the badge would read authenticated.
    let Some(clean) = strip_comments_and_quotes(mechanisms) else {
        return unknown();
    };
    let clean = clean.to_ascii_lowercase();
    TrustSignals {
        spf: result_for(&clean, "spf"),
        dkim: result_for(&clean, "dkim"),
        dmarc: result_for(&clean, "dmarc"),
        authserv_id: Some(authserv_id.to_owned()),
    }
}

/// `Received-SPF` (RFC 7208 §9) carries no `authserv-id` field, so this
/// applies the same "is this our own provider" gate twice: the header must sit
/// above the first `Received:` header, and its verifier identity (`receiver=`,
/// else the leading host of the first comment, e.g.
/// "pass (mail.example.com: domain of ... designates ...)") must match
/// `provider_host` through `authserv_matches`.
fn from_received_spf(raw: &[u8], provider_host: &str) -> TrustSignals {
    let Some(value) = first_received_spf_above_received(raw) else {
        return TrustSignals::default();
    };
    // The verifier identity comes ONLY from `receiver=` or the leading host
    // token of the first comment, never from a substring of the text, and must
    // match the account's host like an authserv-id does.
    let Some(identity) = received_spf_identity(&value) else {
        return TrustSignals::default();
    };
    if provider_host.is_empty() || !authserv_matches(&identity, provider_host) {
        return TrustSignals::default();
    }
    // Fail closed on an unterminated comment or quote, as above.
    let Some(clean) = strip_comments_and_quotes(&value) else {
        return TrustSignals::default();
    };
    let verdict = clean
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    TrustSignals {
        spf: token_to_result(&verdict),
        dkim: TrustResult::None,
        dmarc: TrustResult::None,
        authserv_id: None,
    }
}

/// The first `Received-SPF` header value (unfolded) that sits ABOVE the first
/// `Received:` header in document order. Our own provider prepends its trace
/// headers on top, so anything below a `Received:` line came from upstream and
/// may be forged by the sender; when in doubt, `None`.
fn first_received_spf_above_received(raw: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw);
    let mut found: Option<String> = None;
    let mut current: Option<String> = None;
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            break; // end of the header block
        }
        if line.starts_with([' ', '\t']) {
            if let Some(v) = current.as_mut() {
                v.push(' ');
                v.push_str(line.trim());
            }
            continue;
        }
        if let Some(v) = current.take() {
            found = Some(v);
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("received") {
            return None;
        }
        if name.eq_ignore_ascii_case("received-spf") {
            current = Some(value.trim().to_owned());
        }
    }
    found.or(current)
}

/// Verifier host named by a `Received-SPF` value: `receiver=host` if present,
/// else the leading token of the first parenthetical comment up to `:`/space.
fn received_spf_identity(value: &str) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    let stripped = strip_comments_and_quotes(&lower)?;
    if let Some(pos) = stripped.find("receiver=") {
        let id: String = stripped[pos + "receiver=".len()..]
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != ';')
            .collect();
        return (!id.is_empty()).then_some(id);
    }
    let open = lower.find('(')?;
    let id: String = lower[open + 1..]
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != ':' && *c != ')')
        .collect();
    (!id.is_empty()).then_some(id)
}

/// Remove RFC 5322 comments (nested parentheses, backslash escapes) and quoted
/// strings from header text, replacing each with one space. Both can carry
/// sender-controlled text (the envelope-from local part may legally contain
/// `=`, `;` and parentheses), so no verdict may ever be read from inside them.
/// Returns `None` when the text ends inside a comment, a quoted string or a
/// backslash escape: such a header has no certain structure, and silently
/// swallowing the rest would DROP any `dmarc=fail` that follows an unbalanced
/// "(" while keeping the `pass` before it (#3472). Callers treat `None` as
/// "no verdict at all".
fn strip_comments_and_quotes(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let (mut depth, mut quoted, mut escaped) = (0usize, false, false);
    for c in s.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if depth > 0 || quoted => escaped = true,
            '"' if depth == 0 => {
                if quoted {
                    out.push(' ');
                }
                quoted = !quoted;
            }
            _ if quoted => {}
            '(' => depth += 1,
            ')' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    out.push(' ');
                }
            }
            _ if depth > 0 => {}
            _ => out.push(c),
        }
    }
    (depth == 0 && !quoted && !escaped).then_some(out)
}

/// Read the verdict for `mechanism` from the `Authentication-Results` "resinfo"
/// text (already stripped of comments and quoted strings, and lowercased, by
/// the caller), structurally: the text is
/// split on `;`, and a mechanism counts only when a segment STARTS with
/// `<mechanism>=` (never mid-segment, never inside a property value such as
/// `smtp.mailfrom=`). If it appears more than once with different verdicts the
/// worst wins (Fail over None over Pass).
fn result_for(clean: &str, mechanism: &str) -> TrustResult {
    let needle = format!("{mechanism}=");
    let mut worst: Option<TrustResult> = None;
    for seg in clean.split(';') {
        let Some(rest) = seg.trim().strip_prefix(needle.as_str()) else {
            continue;
        };
        let token: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect();
        let r = token_to_result(&token);
        let rank = |t: &TrustResult| match t {
            TrustResult::Fail => 2,
            TrustResult::None => 1,
            _ => 0,
        };
        if worst.as_ref().is_none_or(|w| rank(&r) > rank(w)) {
            worst = Some(r);
        }
    }
    worst.unwrap_or(TrustResult::None)
}

fn token_to_result(token: &str) -> TrustResult {
    match token {
        "pass" => TrustResult::Pass,
        "fail" | "softfail" | "hardfail" => TrustResult::Fail,
        // "neutral" | "none" | "policy" | "temperror" | "permerror" | "" | unknown
        _ => TrustResult::None,
    }
}

/// Providers known to stamp their OWN Authentication-Results header on every
/// inbound message, as (IMAP host suffix, authserv-id suffix). Deliberately
/// short and closed: a provider belongs here only when both facts are known
/// (it always stamps, and this is how its verifying MTA names itself).
const KNOWN_PROVIDERS: &[(&str, &str)] = &[
    ("gmail.com", "mx.google.com"),
    ("googlemail.com", "mx.google.com"),
    ("outlook.com", "protection.outlook.com"),
    ("office365.com", "protection.outlook.com"),
    ("hotmail.com", "protection.outlook.com"),
    ("fastmail.com", "messagingengine.com"),
];

/// True only when `provider_host` (the account's IMAP host) and `authserv_id`
/// are a pair from `KNOWN_PROVIDERS`, each matched on a DNS label boundary.
///
/// There is deliberately NO "authserv-id equals / is a parent or child of the
/// IMAP host" rule any more (#3472): on a server that stamps no header of its
/// own, the sender's forged header is the topmost one and the IMAP host name
/// is guessable, so that rule let `Authentication-Results: <imap host>;
/// spf=pass; dkim=pass; dmarc=pass` read as a full Pass. An unlisted provider
/// reads `None`: a missed Pass is the safe failure mode, a false Pass is not.
fn authserv_matches(authserv_id: &str, provider_host: &str) -> bool {
    let id = authserv_id
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let host = provider_host
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if id.is_empty() || host.is_empty() {
        return false;
    }
    KNOWN_PROVIDERS
        .iter()
        .any(|(host_suffix, authserv_suffix)| {
            let on_boundary =
                |s: &str, suffix: &str| s == suffix || s.ends_with(&format!(".{suffix}"));
            on_boundary(&host, host_suffix) && on_boundary(&id, authserv_suffix)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(headers: &str, body: &str) -> Vec<u8> {
        format!("From: a@example.com\r\nTo: b@example.com\r\nSubject: t\r\n{headers}\r\n\r\n{body}")
            .into_bytes()
    }

    #[test]
    fn outermost_header_from_own_provider_yields_pass() {
        let raw = msg(
            "Authentication-Results: mx1.messagingengine.com; spf=pass smtp.mailfrom=a@example.com; dkim=pass header.d=example.com; dmarc=pass\r\n",
            "body",
        );
        let signals = extract_trust_signals(&raw, "imap.fastmail.com");
        assert_eq!(signals.spf, TrustResult::Pass);
        assert_eq!(signals.dkim, TrustResult::Pass);
        assert_eq!(signals.dmarc, TrustResult::Pass);
        assert_eq!(
            signals.authserv_id.as_deref(),
            Some("mx1.messagingengine.com")
        );
    }

    /// THE mandatory "no false pass" test: a header whose authserv-id does
    /// NOT match the account's own provider — exactly what an attacker who
    /// crafts their own message can produce — must never read as Pass.
    #[test]
    fn forged_authserv_id_never_yields_a_pass() {
        let raw = msg(
            "Authentication-Results: evil.example; spf=pass dkim=pass dmarc=pass\r\n",
            "body",
        );
        let signals = extract_trust_signals(&raw, "imap.fastmail.com");
        assert_eq!(signals.spf, TrustResult::None);
        assert_eq!(signals.dkim, TrustResult::None);
        assert_eq!(signals.dmarc, TrustResult::None);
        // Named for the details popover, but never trusted for a verdict.
        assert_eq!(signals.authserv_id.as_deref(), Some("evil.example"));
    }

    /// A message author who prepends their OWN forged header still loses,
    /// because a real hop (GreenMail's own delivery, mirrored here as a
    /// second Authentication-Results header BELOW the attacker's, matching
    /// document order after transit) is what `.first()` — i.e. topmost —
    /// actually returns. Only the topmost is ever read.
    #[test]
    fn only_the_outermost_of_multiple_headers_is_trusted() {
        let raw = msg(
            "Authentication-Results: mx1.messagingengine.com; spf=pass dkim=pass dmarc=pass\r\nAuthentication-Results: evil.example; spf=pass dkim=pass dmarc=pass\r\n",
            "body",
        );
        let signals = extract_trust_signals(&raw, "imap.fastmail.com");
        assert_eq!(signals.spf, TrustResult::Pass);
        assert_eq!(
            signals.authserv_id.as_deref(),
            Some("mx1.messagingengine.com")
        );
    }

    #[test]
    fn fail_and_softfail_map_to_fail() {
        let raw = msg(
            "Authentication-Results: mx1.messagingengine.com; spf=fail; dkim=softfail; dmarc=fail\r\n",
            "body",
        );
        let signals = extract_trust_signals(&raw, "imap.fastmail.com");
        assert_eq!(signals.spf, TrustResult::Fail);
        assert_eq!(signals.dkim, TrustResult::Fail);
        assert_eq!(signals.dmarc, TrustResult::Fail);
    }

    #[test]
    fn neutral_temperror_and_missing_mechanisms_map_to_none() {
        let raw = msg(
            "Authentication-Results: mx1.messagingengine.com; spf=neutral; dkim=temperror\r\n",
            "body",
        );
        let signals = extract_trust_signals(&raw, "imap.fastmail.com");
        assert_eq!(signals.spf, TrustResult::None);
        assert_eq!(signals.dkim, TrustResult::None);
        assert_eq!(signals.dmarc, TrustResult::None, "dmarc= absent entirely");
    }

    #[test]
    fn forged_verdicts_inside_comments_and_quotes_are_ignored() {
        let raw = msg(
            "Authentication-Results: mx.google.com; spf=pass (google.com: domain of dmarc=pass@evil.example designates 1.2.3.4 as permitted sender) smtp.mailfrom=\"dkim=pass@evil.example\"; dmarc=fail (p=NONE) header.from=bank.example\r\n",
            "body",
        );
        let s = extract_trust_signals(&raw, "imap.gmail.com");
        assert_eq!(s.spf, TrustResult::Pass);
        assert_eq!(s.dkim, TrustResult::None);
        assert_eq!(s.dmarc, TrustResult::Fail);
    }

    #[test]
    fn dkim_pass_only_in_a_comment_reads_none() {
        let raw = msg(
            "Authentication-Results: mx.google.com; spf=pass (note: dkim=pass); dmarc=pass\r\n",
            "body",
        );
        let s = extract_trust_signals(&raw, "imap.gmail.com");
        assert_eq!(s.dkim, TrustResult::None);
        assert_eq!(s.dmarc, TrustResult::Pass);
    }

    #[test]
    fn conflicting_duplicate_mechanisms_take_the_worst() {
        let raw = msg(
            "Authentication-Results: mx.google.com; dkim=pass header.i=@a.com; dkim=fail header.i=@b.com\r\n",
            "body",
        );
        assert_eq!(
            extract_trust_signals(&raw, "imap.gmail.com").dkim,
            TrustResult::Fail
        );
    }

    #[test]
    fn normal_gmail_header_still_reads_pass() {
        let raw = msg(
            "Authentication-Results: mx.google.com;\r\n       dkim=pass header.i=@x.com header.s=s1;\r\n       spf=pass (google.com: domain of a@x.com designates 1.2.3.4 as permitted sender) smtp.mailfrom=a@x.com;\r\n       dmarc=pass (p=REJECT sp=REJECT dis=NONE) header.from=x.com\r\n",
            "body",
        );
        let s = extract_trust_signals(&raw, "imap.gmail.com");
        assert_eq!(
            (s.spf, s.dkim, s.dmarc),
            (TrustResult::Pass, TrustResult::Pass, TrustResult::Pass)
        );
    }

    #[test]
    fn received_spf_receiver_inside_a_comment_is_ignored() {
        let raw = msg(
            "Received-SPF: pass (evil.example: receiver=imap.fastmail.com) client-ip=1.2.3.4\r\n",
            "body",
        );
        assert_eq!(
            extract_trust_signals(&raw, "imap.fastmail.com").spf,
            TrustResult::None
        );
    }

    #[test]
    fn alias_table_requires_a_label_boundary() {
        // "evilmessagingengine.com" is a registrable look-alike, not a
        // subdomain of messagingengine.com.
        let raw = msg(
            "Authentication-Results: evilmessagingengine.com; spf=pass dkim=pass dmarc=pass\r\n",
            "body",
        );
        let signals = extract_trust_signals(&raw, "imap.fastmail.com");
        assert_eq!(signals.spf, TrustResult::None);
        assert_eq!(signals.dkim, TrustResult::None);
        // The genuine subdomain still passes.
        let ok = msg(
            "Authentication-Results: mx1.messagingengine.com; spf=pass\r\n",
            "body",
        );
        assert_eq!(
            extract_trust_signals(&ok, "imap.fastmail.com").spf,
            TrustResult::Pass
        );
    }

    #[test]
    fn provider_alias_table_matches_gmail() {
        let raw = msg(
            "Authentication-Results: mx.google.com; spf=pass dkim=pass dmarc=pass\r\n",
            "body",
        );
        let signals = extract_trust_signals(&raw, "imap.gmail.com");
        assert_eq!(signals.spf, TrustResult::Pass);
    }

    #[test]
    fn received_spf_fallback_when_no_authentication_results() {
        let raw = msg(
            "Received-SPF: pass (mx1.messagingengine.com: domain of a@example.com designates 1.2.3.4 as permitted sender)\r\n",
            "body",
        );
        let signals = extract_trust_signals(&raw, "imap.fastmail.com");
        assert_eq!(signals.spf, TrustResult::Pass);
        assert_eq!(signals.dkim, TrustResult::None);
    }

    #[test]
    fn received_spf_from_an_unrelated_server_is_ignored() {
        let raw = msg("Received-SPF: pass (evil.example: ...)\r\n", "body");
        let signals = extract_trust_signals(&raw, "imap.fastmail.com");
        assert_eq!(signals.spf, TrustResult::None);
    }

    #[test]
    fn no_headers_at_all_is_all_none_and_does_not_panic() {
        let raw = msg("", "body");
        let signals = extract_trust_signals(&raw, "imap.fastmail.com");
        assert_eq!(signals, TrustSignals::default());
    }

    #[test]
    fn malformed_and_empty_input_never_panics() {
        assert_eq!(extract_trust_signals(b"", "host"), TrustSignals::default());
        assert_eq!(
            extract_trust_signals(b"\x00\x01\x02 not an email", "host"),
            TrustSignals::default()
        );
        // A header with no ';' at all — must not panic on split_once.
        let raw = msg("Authentication-Results: garbage-no-semicolon\r\n", "body");
        let signals = extract_trust_signals(&raw, "garbage-no-semicolon");
        assert_eq!(signals, TrustSignals::default());
    }

    #[test]
    fn empty_provider_host_never_matches() {
        // A caller bug (empty host) must fail closed, not accidentally match
        // every authserv-id via an empty-string `ends_with`.
        let raw = msg("Authentication-Results: ; spf=pass\r\n", "body");
        let signals = extract_trust_signals(&raw, "");
        assert_eq!(signals, TrustSignals::default());
    }

    #[test]
    fn authserv_id_matches_only_known_provider_pairs() {
        assert!(!authserv_matches("com", "imap.example.com"));
        assert!(!authserv_matches("1", "127.0.0.1"));
        assert!(!authserv_matches("co.uk", "imap.example.co.uk"));
        assert!(!authserv_matches("evil.com", "com"));
        // #3472: host equality and parent/child domains no longer vouch.
        assert!(!authserv_matches("fastmail.com", "imap.fastmail.com"));
        assert!(!authserv_matches("imap.fastmail.com", "imap.fastmail.com"));
        assert!(!authserv_matches("mx.example.co.uk", "mx.example.co.uk"));
        assert!(!authserv_matches("example.co.uk", "imap.example.co.uk"));
        assert!(!authserv_matches("127.0.0.1", "127.0.0.1"));
        // A known authserv-id does not vouch for an unrelated account host.
        assert!(!authserv_matches("mx.google.com", "mail.example.org"));
        assert!(!authserv_matches("mx.google.com", "imap.fastmail.com"));
        // Known pairs, on a label boundary, case and trailing dot ignored.
        assert!(authserv_matches("mx.google.com", "imap.gmail.com"));
        assert!(authserv_matches("MX.Google.com.", "imap.gmail.com"));
        assert!(authserv_matches(
            "mx1.messagingengine.com",
            "imap.fastmail.com"
        ));
        assert!(authserv_matches(
            "dm6nam10.protection.outlook.com",
            "outlook.office365.com"
        ));
    }

    /// #3472 item 2, the reviewer's exact input: the account's provider stamps
    /// no Authentication-Results, so the sender's own header is topmost, and
    /// it names the (guessable) IMAP host as authserv-id.
    #[test]
    fn forged_pass_naming_the_account_host_reads_unknown() {
        for host in ["mail.example.org", "imap.example.co.uk", "127.0.0.1"] {
            let raw = msg(
                &format!("Authentication-Results: {host}; spf=pass; dkim=pass; dmarc=pass\r\n"),
                "body",
            );
            let s = extract_trust_signals(&raw, host);
            assert_eq!(
                (s.spf, s.dkim, s.dmarc),
                (TrustResult::None, TrustResult::None, TrustResult::None),
                "{host}"
            );
            assert_eq!(s.authserv_id.as_deref(), Some(host));
            // Parent-domain flavour of the same forgery.
            let parent = host.split_once('.').map_or(host, |(_, p)| p);
            let raw = msg(
                &format!("Authentication-Results: {parent}; spf=pass; dkim=pass; dmarc=pass\r\n"),
                "body",
            );
            let s = extract_trust_signals(&raw, host);
            assert_eq!(
                (s.spf, s.dkim, s.dmarc),
                (TrustResult::None, TrustResult::None, TrustResult::None),
                "{parent} for {host}"
            );
        }
        // Same forgery through the Received-SPF fallback.
        let raw = msg(
            "Received-SPF: pass receiver=mail.example.org; client-ip=1.2.3.4\r\n",
            "body",
        );
        assert_eq!(
            extract_trust_signals(&raw, "mail.example.org").spf,
            TrustResult::None
        );
    }

    /// #3472 item 1: an unbalanced "(" used to swallow the rest of the header,
    /// dropping the `dmarc=fail` after it while keeping the `spf=pass` before
    /// it. A header that ends inside a comment or quote yields no verdict.
    #[test]
    fn unterminated_comment_or_quote_yields_no_verdict_at_all() {
        for header in [
            "mx.google.com; spf=pass (google.com: domain of a@evil.example; dkim=pass header.d=evil.example; dmarc=fail (p=REJECT) header.from=bank.example",
            "mx.google.com; spf=pass smtp.mailfrom=a@evil.example; dkim=pass ( ; dmarc=fail header.from=bank.example",
            "mx.google.com; spf=pass; dkim=pass (nested (still open); dmarc=fail",
            "mx.google.com; spf=pass smtp.mailfrom=\"a@evil.example; dkim=pass; dmarc=fail",
            "mx.google.com; spf=pass; dkim=pass; dmarc=fail (escaped close \\)",
        ] {
            let raw = msg(&format!("Authentication-Results: {header}\r\n"), "body");
            let s = extract_trust_signals(&raw, "imap.gmail.com");
            assert_eq!(
                (s.spf, s.dkim, s.dmarc),
                (TrustResult::None, TrustResult::None, TrustResult::None),
                "{header}"
            );
            assert_eq!(s.authserv_id.as_deref(), Some("mx.google.com"));
        }
        let balanced = strip_comments_and_quotes("a (b (c)) \"d\" e").expect("balanced");
        assert_eq!(balanced.split_whitespace().collect::<Vec<_>>(), ["a", "e"]);
        assert_eq!(strip_comments_and_quotes("a (b"), None);
        assert_eq!(strip_comments_and_quotes("a (b (c)"), None);
        assert_eq!(strip_comments_and_quotes("a \"b"), None);
        assert_eq!(strip_comments_and_quotes("a (b \\"), None);
        // Received-SPF with an unterminated comment: no verdict either.
        let raw = msg(
            "Received-SPF: pass receiver=mx1.messagingengine.com (unterminated\r\n",
            "body",
        );
        assert_eq!(
            extract_trust_signals(&raw, "imap.fastmail.com").spf,
            TrustResult::None
        );
    }

    #[test]
    fn forged_authentication_results_with_bare_tld_reads_none() {
        let raw = msg(
            "Authentication-Results: com; spf=pass; dkim=pass; dmarc=pass\r\n",
            "body",
        );
        let signals = extract_trust_signals(&raw, "imap.example.com");
        assert_eq!(signals.spf, TrustResult::None);
        assert_eq!(signals.dkim, TrustResult::None);
        assert_eq!(signals.dmarc, TrustResult::None);
    }

    #[test]
    fn received_spf_forged_comment_naming_host_is_ignored() {
        let raw = msg(
            "Received: from mx.real.example by imap.fastmail.com; Mon, 1 Jan 2026 00:00:00 +0000\r\nReceived-SPF: pass (attacker says imap.fastmail.com is fine) client-ip=6.6.6.6;\r\n",
            "body",
        );
        let signals = extract_trust_signals(&raw, "imap.fastmail.com");
        assert_eq!(signals.spf, TrustResult::None);
    }

    #[test]
    fn received_spf_below_the_first_received_header_is_ignored() {
        let raw = msg(
            "Received: from mx.real.example by imap.fastmail.com; Mon, 1 Jan 2026 00:00:00 +0000\r\nReceived-SPF: pass (imap.fastmail.com: domain of a@example.com designates 1.2.3.4 as permitted sender)\r\n",
            "body",
        );
        let signals = extract_trust_signals(&raw, "imap.fastmail.com");
        assert_eq!(signals.spf, TrustResult::None);
    }

    #[test]
    fn received_spf_above_received_with_receiver_token_passes() {
        let raw = msg(
            "Received-SPF: pass receiver=mx1.messagingengine.com; client-ip=1.2.3.4\r\nReceived: from mx.real.example by imap.fastmail.com; Mon, 1 Jan 2026 00:00:00 +0000\r\n",
            "body",
        );
        let signals = extract_trust_signals(&raw, "imap.fastmail.com");
        assert_eq!(signals.spf, TrustResult::Pass);
    }
}
