// Castellum - link inspection for the reading-pane hover reveal (S48).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Pure, side-effect-free analysis of one (href, visible link text) pair, run
// on demand when the pointer is actually over a link (see EmailBodyWebView's
// isolated-world hover script). Never panics on a malformed href — an
// attacker fully controls both `href` and the visible text of any link in a
// received message.

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LinkInspection {
    /// The href as written, verbatim (for display/copy).
    pub href: String,
    pub scheme: Option<String>,
    /// The ASCII/punycode host, when the href parses as an absolute URL with one.
    pub display_host: Option<String>,
    /// The Unicode rendering of `display_host`, populated ONLY when
    /// `idn_caution` is also true — i.e. only when there is something to show.
    pub unicode_host: Option<String>,
    /// True when any label of the host is punycode-encoded (starts `xn--`).
    /// Not a homograph-attack detector on its own — a legitimate IDN domain
    /// looks the same — but flags "this host isn't plain ASCII" so the UI can
    /// show both forms and let a human judge it, rather than silently
    /// rendering a look-alike.
    pub idn_caution: bool,
    /// True when the VISIBLE link text itself reads as a URL or bare domain
    /// naming a DIFFERENT host than `href` actually points to — the classic
    /// "https://yourbank.com" (text) -> evil.example (href) trick. Ordinary
    /// prose link text ("click here") never sets this.
    pub text_mismatch: bool,
}

/// Analyse one link. Never panics: an unparseable `href` (relative path,
/// garbage, empty string) yields `display_host: None` and `text_mismatch:
/// false` rather than an error — the caller (Swift) always has something
/// sane to show.
pub fn inspect_link(href: &str, visible_text: &str) -> LinkInspection {
    let href = href.trim();
    let parsed = url::Url::parse(href).ok();
    let scheme = parsed.as_ref().map(|u| u.scheme().to_owned());
    let display_host = parsed
        .as_ref()
        .and_then(|u| u.host_str())
        .map(str::to_owned);

    let (unicode_host, idn_caution) = match &display_host {
        Some(host) => {
            // `domain_to_unicode` returns `Result<(), Errors>`, not a bool —
            // `Ok(())` means the decode succeeded cleanly.
            let (unicode, decode_result) = idna::domain_to_unicode(host);
            let is_punycode = host.split('.').any(|label| label.starts_with("xn--"));
            if is_punycode && decode_result.is_ok() {
                (Some(unicode), true)
            } else {
                (None, is_punycode)
            }
        }
        None => (None, false),
    };

    let text_mismatch = display_host
        .as_deref()
        .is_some_and(|real_host| text_names_a_different_host(visible_text, real_host));

    LinkInspection {
        href: href.to_owned(),
        scheme,
        display_host,
        unicode_host,
        idn_caution,
        text_mismatch,
    }
}

/// Invisible or direction-changing code points a sender can sprinkle into link
/// text (the hover script sends `textContent`, which includes hidden spans):
/// zero-width space/joiners, bidi controls, word joiner, BOM, soft hyphen.
fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{034F}' | '\u{061C}' | '\u{180E}'
        | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{206F}' | '\u{FEFF}' | '\u{FE00}'..='\u{FE0F}')
        || c.is_control()
}

/// Host a single whitespace-free token claims to be, if it reads as a URL or a
/// domain (optionally followed by a port, path, query or fragment).
/// `strict` (token is part of longer prose) only accepts an explicit scheme or
/// a leading `www.`, so words such as `invoice.pdf` are never flagged.
fn claimed_host_of(token: &str, strict: bool) -> Option<String> {
    let token = token.trim_matches(|c: char| {
        matches!(
            c,
            '(' | ')' | '<' | '>' | '[' | ']' | '"' | '\'' | ',' | ';' | '.' | ':' | '!'
        )
    });
    if token.is_empty() {
        return None;
    }
    if let Some((scheme, _)) = token.split_once("://") {
        if scheme.chars().all(|c| c.is_ascii_alphabetic()) {
            return url::Url::parse(token).ok()?.host_str().map(str::to_owned);
        }
    }
    if strict && !token.to_ascii_lowercase().starts_with("www.") {
        return None;
    }
    // Bare domain, possibly with :port/path/?query/#fragment after it.
    let host_part = token.split(['/', '?', '#']).next().unwrap_or("");
    let host_part = host_part.rsplit_once(':').map_or(host_part, |(h, port)| {
        if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) {
            h
        } else {
            host_part
        }
    });
    if !looks_like_bare_domain(host_part) {
        return None;
    }
    url::Url::parse(&format!("http://{host_part}"))
        .ok()?
        .host_str()
        .map(str::to_owned)
}

/// True when `visible_text` itself reads as (or contains) a URL or bare domain
/// whose host does not match `real_host`. Invisible characters are removed
/// first, and `domain.tld/path`, `domain.tld:443` and a URL embedded in a
/// sentence are all recognised (audit #3371).
fn text_names_a_different_host(visible_text: &str, real_host: &str) -> bool {
    let cleaned: String = visible_text
        .chars()
        .filter(|c| !is_invisible(*c) || c.is_whitespace())
        .collect();
    let tokens: Vec<&str> = cleaned.split_whitespace().collect();
    let strict = tokens.len() > 1;
    tokens
        .iter()
        .filter_map(|t| claimed_host_of(t, strict))
        .any(|claimed| !hosts_match(&claimed, real_host))
}

/// Cheap, deliberately conservative "does this read as a bare domain"
/// check - a single token, no whitespace, at least one dot, alphanumeric or
/// hyphen labels (Unicode allowed, so a lookalike-letter domain is still
/// compared against the real host), ending in a plausible (2+ letter) TLD.
/// Never matches ordinary prose.
fn looks_like_bare_domain(text: &str) -> bool {
    if text.is_empty() || text.contains(char::is_whitespace) {
        return false;
    }
    let Some((_, tld)) = text.rsplit_once('.') else {
        return false;
    };
    tld.chars().count() >= 2
        && tld.chars().all(char::is_alphabetic)
        && text
            .chars()
            .all(|c| c.is_alphanumeric() || c == '.' || c == '-')
}

fn hosts_match(a: &str, b: &str) -> bool {
    let a = a.trim_end_matches('.').to_ascii_lowercase();
    let b = b.trim_end_matches('.').to_ascii_lowercase();
    a == b || a.ends_with(&format!(".{b}")) || b.ends_with(&format!(".{a}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_text_and_href_has_no_mismatch() {
        let i = inspect_link("https://example.com/page", "https://example.com/page");
        assert!(!i.text_mismatch);
        assert_eq!(i.display_host.as_deref(), Some("example.com"));
    }

    #[test]
    fn bare_domain_text_pointing_elsewhere_is_a_mismatch() {
        let i = inspect_link("https://evil.example/phish", "yourbank.com");
        assert!(i.text_mismatch);
    }

    #[test]
    fn full_url_text_pointing_elsewhere_is_a_mismatch() {
        let i = inspect_link("https://evil.example/phish", "https://yourbank.com/login");
        assert!(i.text_mismatch);
    }

    #[test]
    fn subdomain_of_the_claimed_host_is_not_a_mismatch() {
        let i = inspect_link("https://mail.example.com/x", "example.com");
        assert!(!i.text_mismatch);
    }

    #[test]
    fn ordinary_prose_text_is_never_flagged() {
        let i = inspect_link(
            "https://evil.example/phish",
            "click here to view your invoice",
        );
        assert!(!i.text_mismatch);
    }

    #[test]
    fn punycode_host_sets_idn_caution_and_unicode_host() {
        // xn--80ak6aa92e.com decodes to a Cyrillic-lookalike "apple.com".
        let i = inspect_link("https://xn--80ak6aa92e.com/login", "apple.com");
        assert!(i.idn_caution);
        assert!(i.unicode_host.is_some());
    }

    #[test]
    fn plain_ascii_host_has_no_idn_caution() {
        let i = inspect_link("https://example.com", "example.com");
        assert!(!i.idn_caution);
        assert_eq!(i.unicode_host, None);
    }

    #[test]
    fn mailto_and_tel_have_no_host_and_never_panic() {
        for href in ["mailto:a@b.com", "tel:+441223000000"] {
            let i = inspect_link(href, "contact us");
            assert_eq!(i.display_host, None);
            assert!(!i.text_mismatch);
        }
    }

    #[test]
    fn malformed_or_relative_href_never_panics() {
        for href in [
            "",
            "not a url at all",
            "/relative/path",
            "javascript:alert(1)",
        ] {
            let i = inspect_link(href, "click");
            assert_eq!(i.href, href.trim());
            // javascript: parses with a scheme but no host — must still not panic.
            let _ = i.display_host;
        }
    }

    #[test]
    fn domain_with_path_or_port_pointing_elsewhere_is_a_mismatch() {
        for text in [
            "www.yourbank.com/login",
            "yourbank.com:443/x",
            "yourbank.com/a?b=c#d",
        ] {
            assert!(
                inspect_link("https://evil.example/login", text).text_mismatch,
                "{text}"
            );
        }
    }

    #[test]
    fn invisible_characters_do_not_hide_the_claimed_host() {
        for text in [
            "yourbank.com\u{200B}",
            "your\u{200B}bank.com",
            "\u{202E}yourbank.com",
            "yourbank.com\u{00AD}",
        ] {
            assert!(
                inspect_link("https://evil.example/", text).text_mismatch,
                "{text:?}"
            );
        }
    }

    #[test]
    fn url_embedded_in_a_sentence_is_checked() {
        let i = inspect_link(
            "https://evil.example/",
            "Sign in at https://yourbank.com now",
        );
        assert!(i.text_mismatch);
        let i = inspect_link(
            "https://evil.example/",
            "Sign in at www.yourbank.com/login now",
        );
        assert!(i.text_mismatch);
        // a matching embedded URL, or a plain filename, is fine
        assert!(
            !inspect_link(
                "https://yourbank.com/x",
                "Sign in at https://yourbank.com now"
            )
            .text_mismatch
        );
        assert!(
            !inspect_link("https://evil.example/", "see invoice.pdf for details").text_mismatch
        );
    }

    #[test]
    fn lookalike_unicode_domain_text_is_a_mismatch() {
        // Cyrillic "a" in the claimed domain; the real host is plain ASCII.
        assert!(inspect_link("https://yourbank.com/", "yourb\u{430}nk.com").text_mismatch);
    }

    #[test]
    fn empty_visible_text_is_never_a_mismatch() {
        let i = inspect_link("https://example.com", "");
        assert!(!i.text_mismatch);
    }
}
