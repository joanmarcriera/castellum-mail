// Castellum - Thunderbird "autoconfig" (config-v1.1.xml) reader (MM-14, slice 1).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Input is UNTRUSTED: it comes from a domain's own web host or from Mozilla's
// ISPDB. `roxmltree` is read-only, rejects DTDs (so no entity expansion), and
// is given a node budget; every value taken from the document is validated
// before it can reach the sheet. Fuzzed by
// `native/fuzz/fuzz_targets/autoconfig_xml_decode.rs`.
//
// Selection rules (see account-autodetect.md §3.2 and DECISIONS):
//   * IMAP must be password-authenticated AND encrypted. Implicit TLS (`SSL`)
//     is preferred over `STARTTLS`; a plaintext socket is never accepted.
//   * SMTP is the first `SSL`/`STARTTLS` server whose security matches what the
//     core's SMTP sender will actually do (implicit TLS iff port 465).
//   * `%EMAILDOMAIN%` in a hostname becomes the request domain; a hostname
//     naming the local part is invalid (the local part is never known here).

use super::{Endpoint, Found, TlsMode, UsernameForm};
use roxmltree::{Document, Node, ParsingOptions};

/// Largest document accepted (also enforced by the HTTP transport).
pub(crate) const MAX_BODY: usize = 64 * 1024;
const MAX_NODES: u32 = 4_000;
const MAX_DISPLAY_NAME: usize = 64;

/// What a document says.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum XmlOutcome {
    /// Usable IMAP (and perhaps SMTP) settings.
    Found(Found),
    /// IMAP exists but only with OAuth2 authentication.
    OAuthOnly,
    /// IMAP exists but only on a plaintext socket.
    NoTlsImap,
    /// Parsed, but no IMAP server (e.g. POP3-only) -- a miss.
    NoImap,
    /// Not XML, not an autoconfig document, or a value failed validation.
    Invalid,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Socket {
    Ssl,
    StartTls,
    Plain,
    Other,
}

struct Server {
    host: String,
    port: Option<u16>,
    socket: Socket,
    password_auth: bool,
    oauth: bool,
    username: Option<UsernameForm>,
}

/// True for a syntactically valid public-looking DNS hostname: labels of
/// `[a-z0-9-]` (1..=63, no edge hyphen), at least one dot, <= 253 bytes, not an
/// IP literal, not `localhost`.
pub(crate) fn valid_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 || host.eq_ignore_ascii_case("localhost") {
        return false;
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    for l in &labels {
        let b = l.as_bytes();
        if b.is_empty()
            || b.len() > 63
            || b[0] == b'-'
            || b[b.len() - 1] == b'-'
            || !b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-')
        {
            return false;
        }
    }
    // The last label of a real name is never all digits (rejects 1.2.3.4).
    !labels
        .last()
        .is_some_and(|l| l.bytes().all(|c| c.is_ascii_digit()))
}

fn child_text<'a>(n: Node<'a, 'a>, name: &str) -> Option<&'a str> {
    n.children()
        .find(|c| c.is_element() && c.tag_name().name() == name)
        .and_then(|c| c.text())
        .map(str::trim)
}

fn read_server(n: Node, domain: &str) -> Option<Server> {
    let raw_host = child_text(n, "hostname")?.to_ascii_lowercase();
    // `%EMAILDOMAIN%` -> the domain; any other leftover placeholder (notably
    // %EMAILLOCALPART%/%EMAILADDRESS%) fails host validation below.
    let host = raw_host
        .replace("%emaildomain%", domain)
        .trim_end_matches('.')
        .to_string();
    let port = child_text(n, "port").and_then(|p| p.parse::<u16>().ok().filter(|p| *p != 0));
    let socket = match child_text(n, "socketType") {
        Some(s) if s.eq_ignore_ascii_case("SSL") => Socket::Ssl,
        Some(s) if s.eq_ignore_ascii_case("STARTTLS") => Socket::StartTls,
        Some(s) if s.eq_ignore_ascii_case("plain") => Socket::Plain,
        _ => Socket::Other,
    };
    let (mut password_auth, mut oauth) = (false, false);
    for a in n
        .children()
        .filter(|c| c.is_element() && c.tag_name().name() == "authentication")
    {
        match a.text().map(|t| t.trim().to_ascii_lowercase()).as_deref() {
            Some("password-cleartext") | Some("plain") => password_auth = true,
            Some("oauth2") => oauth = true,
            _ => {}
        }
    }
    let username = match child_text(n, "username") {
        Some("%EMAILADDRESS%") => Some(UsernameForm::Email),
        Some("%EMAILLOCALPART%") => Some(UsernameForm::LocalPart),
        _ => None,
    };
    Some(Server {
        host,
        port,
        socket,
        password_auth,
        oauth,
        username,
    })
}

fn endpoint(s: &Server, tls: TlsMode) -> Option<Endpoint> {
    if !valid_host(&s.host) {
        return None;
    }
    Some(Endpoint {
        host: s.host.clone(),
        port: s.port?,
        tls,
    })
}

/// Strip control characters, trim, and cap at 64 characters.
fn clean_display_name(raw: &str) -> Option<String> {
    let s: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_DISPLAY_NAME)
        .collect();
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Parse one config-v1.1.xml document for `domain`.
pub(crate) fn parse(body: &[u8], domain: &str) -> XmlOutcome {
    if body.len() > MAX_BODY {
        return XmlOutcome::Invalid;
    }
    let Ok(text) = std::str::from_utf8(body) else {
        return XmlOutcome::Invalid;
    };
    let opts = ParsingOptions {
        allow_dtd: false,
        nodes_limit: MAX_NODES,
    };
    let Ok(doc) = Document::parse_with_options(text, opts) else {
        return XmlOutcome::Invalid;
    };
    let root = doc.root_element();
    if root.tag_name().name() != "clientConfig" {
        return XmlOutcome::Invalid;
    }
    let Some(provider) = root
        .children()
        .find(|c| c.is_element() && c.tag_name().name() == "emailProvider")
    else {
        return XmlOutcome::Invalid;
    };
    let display_name = child_text(provider, "displayName").and_then(clean_display_name);

    let servers = |tag: &str, ty: &str| -> Vec<Server> {
        provider
            .children()
            .filter(|c| {
                c.is_element() && c.tag_name().name() == tag && c.attribute("type") == Some(ty)
            })
            .filter_map(|c| read_server(c, domain))
            .collect()
    };
    let imaps = servers("incomingServer", "imap");
    let smtps = servers("outgoingServer", "smtp");
    if imaps.is_empty() {
        return XmlOutcome::NoImap;
    }

    // Prefer implicit TLS, then STARTTLS; both need password authentication.
    let pick = [
        (Socket::Ssl, TlsMode::Implicit),
        (Socket::StartTls, TlsMode::Starttls),
    ]
    .into_iter()
    .find_map(|(sock, tls)| {
        imaps
            .iter()
            .find(|s| s.socket == sock && s.password_auth)
            .map(|s| (s, tls))
    });
    let Some((imap_server, imap_tls)) = pick else {
        let encrypted_oauth = imaps
            .iter()
            .any(|s| s.oauth && matches!(s.socket, Socket::Ssl | Socket::StartTls));
        if encrypted_oauth {
            return XmlOutcome::OAuthOnly;
        }
        if imaps.iter().any(|s| s.socket == Socket::Plain) {
            return XmlOutcome::NoTlsImap;
        }
        return XmlOutcome::Invalid;
    };
    let Some(imap) = endpoint(imap_server, imap_tls) else {
        return XmlOutcome::Invalid;
    };

    // SMTP: first encrypted, password-authenticated server whose security
    // agrees with the port convention the core's sender uses.
    let smtp = smtps.iter().find_map(|s| {
        let tls = match s.socket {
            Socket::Ssl => TlsMode::Implicit,
            Socket::StartTls => TlsMode::Starttls,
            _ => return None,
        };
        if !s.password_auth {
            return None;
        }
        let ep = endpoint(s, tls)?;
        super::smtp_consistent(&ep).then_some(ep)
    });

    XmlOutcome::Found(Found {
        display_name,
        imap,
        smtp,
        username: imap_server.username,
        outside_domain: false,
    })
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! Trimmed copies of the document shapes Mozilla's ISPDB serves. Static
    //! strings: no test ever fetches the real thing.

    /// `gmail.com`-shaped: IMAP offers OAuth2 *and* an app password.
    pub const GMAIL: &str = r#"<?xml version="1.0"?>
<clientConfig version="1.1">
  <emailProvider id="googlemail.com">
    <domain>gmail.com</domain>
    <displayName>Google Mail</displayName>
    <displayShortName>GMail</displayShortName>
    <incomingServer type="imap">
      <hostname>imap.gmail.com</hostname>
      <port>993</port>
      <socketType>SSL</socketType>
      <username>%EMAILADDRESS%</username>
      <authentication>oauth2</authentication>
      <authentication>password-cleartext</authentication>
    </incomingServer>
    <incomingServer type="pop3">
      <hostname>pop.gmail.com</hostname>
      <port>995</port>
      <socketType>SSL</socketType>
      <username>%EMAILADDRESS%</username>
      <authentication>password-cleartext</authentication>
    </incomingServer>
    <outgoingServer type="smtp">
      <hostname>smtp.gmail.com</hostname>
      <port>465</port>
      <socketType>SSL</socketType>
      <username>%EMAILADDRESS%</username>
      <authentication>password-cleartext</authentication>
    </outgoingServer>
  </emailProvider>
</clientConfig>"#;

    /// `fastmail.com`-shaped: implicit-TLS IMAP, STARTTLS-or-SSL SMTP.
    pub const FASTMAIL: &str = r#"<?xml version="1.0"?>
<clientConfig version="1.1">
  <emailProvider id="fastmail.com">
    <domain>fastmail.com</domain>
    <displayName>FastMail</displayName>
    <incomingServer type="imap">
      <hostname>imap.fastmail.com</hostname>
      <port>993</port>
      <socketType>SSL</socketType>
      <username>%EMAILADDRESS%</username>
      <authentication>password-cleartext</authentication>
    </incomingServer>
    <outgoingServer type="smtp">
      <hostname>smtp.fastmail.com</hostname>
      <port>587</port>
      <socketType>STARTTLS</socketType>
      <username>%EMAILADDRESS%</username>
      <authentication>password-cleartext</authentication>
    </outgoingServer>
  </emailProvider>
</clientConfig>"#;

    /// Microsoft-365-shaped: IMAP only with OAuth2.
    pub const OAUTH_ONLY: &str = r#"<?xml version="1.0"?>
<clientConfig version="1.1">
  <emailProvider id="example-oauth.test">
    <displayName>Outlook</displayName>
    <incomingServer type="imap">
      <hostname>outlook.office365.com</hostname>
      <port>993</port>
      <socketType>SSL</socketType>
      <username>%EMAILADDRESS%</username>
      <authentication>oauth2</authentication>
    </incomingServer>
    <outgoingServer type="smtp">
      <hostname>smtp.office365.com</hostname>
      <port>587</port>
      <socketType>STARTTLS</socketType>
      <username>%EMAILADDRESS%</username>
      <authentication>oauth2</authentication>
    </outgoingServer>
  </emailProvider>
</clientConfig>"#;

    /// A legacy ISP that only offers plaintext IMAP.
    pub const PLAIN_IMAP_ONLY: &str = r#"<?xml version="1.0"?>
<clientConfig version="1.1">
  <emailProvider id="old-isp.test">
    <incomingServer type="imap">
      <hostname>imap.old-isp.test</hostname>
      <port>143</port>
      <socketType>plain</socketType>
      <username>%EMAILLOCALPART%</username>
      <authentication>password-cleartext</authentication>
    </incomingServer>
  </emailProvider>
</clientConfig>"#;

    /// STARTTLS IMAP with a domain placeholder and a local-part username.
    pub const STARTTLS_PLACEHOLDER: &str = r#"<?xml version="1.0"?>
<clientConfig version="1.1">
  <emailProvider id="isp.test">
    <displayName>Small ISP</displayName>
    <incomingServer type="imap">
      <hostname>imap.%EMAILDOMAIN%</hostname>
      <port>143</port>
      <socketType>STARTTLS</socketType>
      <username>%EMAILLOCALPART%</username>
      <authentication>password-cleartext</authentication>
    </incomingServer>
    <outgoingServer type="smtp">
      <hostname>smtp.%EMAILDOMAIN%</hostname>
      <port>587</port>
      <socketType>STARTTLS</socketType>
      <username>%EMAILLOCALPART%</username>
      <authentication>password-cleartext</authentication>
    </outgoingServer>
  </emailProvider>
</clientConfig>"#;
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn found(o: XmlOutcome) -> Found {
        match o {
            XmlOutcome::Found(f) => f,
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn gmail_shape_yields_password_imap_and_smtp() {
        let f = found(parse(GMAIL.as_bytes(), "gmail.com"));
        assert_eq!(f.display_name.as_deref(), Some("Google Mail"));
        assert_eq!(
            f.imap,
            Endpoint {
                host: "imap.gmail.com".into(),
                port: 993,
                tls: TlsMode::Implicit
            }
        );
        let smtp = f.smtp.unwrap();
        assert_eq!((smtp.host.as_str(), smtp.port), ("smtp.gmail.com", 465));
        assert_eq!(smtp.tls, TlsMode::Implicit);
        assert_eq!(f.username, Some(UsernameForm::Email));
    }

    #[test]
    fn fastmail_shape_yields_starttls_smtp() {
        let f = found(parse(FASTMAIL.as_bytes(), "fastmail.com"));
        assert_eq!(f.imap.host, "imap.fastmail.com");
        let smtp = f.smtp.unwrap();
        assert_eq!((smtp.port, smtp.tls), (587, TlsMode::Starttls));
    }

    #[test]
    fn oauth_only_imap_is_reported_not_filled() {
        assert_eq!(
            parse(OAUTH_ONLY.as_bytes(), "x.test"),
            XmlOutcome::OAuthOnly
        );
    }

    #[test]
    fn plaintext_only_imap_is_never_accepted() {
        assert_eq!(
            parse(PLAIN_IMAP_ONLY.as_bytes(), "old-isp.test"),
            XmlOutcome::NoTlsImap
        );
    }

    #[test]
    fn starttls_imap_is_accepted_and_placeholders_substituted() {
        let f = found(parse(STARTTLS_PLACEHOLDER.as_bytes(), "isp.test"));
        assert_eq!(f.imap.host, "imap.isp.test");
        assert_eq!((f.imap.port, f.imap.tls), (143, TlsMode::Starttls));
        assert_eq!(f.smtp.unwrap().host, "smtp.isp.test");
        assert_eq!(f.username, Some(UsernameForm::LocalPart));
    }

    #[test]
    fn implicit_tls_beats_starttls_regardless_of_document_order() {
        let doc = r#"<clientConfig><emailProvider id="a.test">
          <incomingServer type="imap"><hostname>starttls.a.test</hostname><port>143</port>
            <socketType>STARTTLS</socketType><authentication>password-cleartext</authentication></incomingServer>
          <incomingServer type="imap"><hostname>ssl.a.test</hostname><port>993</port>
            <socketType>SSL</socketType><authentication>password-cleartext</authentication></incomingServer>
        </emailProvider></clientConfig>"#;
        let f = found(parse(doc.as_bytes(), "a.test"));
        assert_eq!(f.imap.host, "ssl.a.test");
    }

    #[test]
    fn local_part_placeholder_in_a_hostname_is_invalid() {
        let doc = STARTTLS_PLACEHOLDER.replace("imap.%EMAILDOMAIN%", "%EMAILLOCALPART%.evil.test");
        assert_eq!(parse(doc.as_bytes(), "isp.test"), XmlOutcome::Invalid);
    }

    #[test]
    fn unknown_username_template_is_none() {
        let doc = FASTMAIL.replace("%EMAILADDRESS%", "admin");
        assert_eq!(found(parse(doc.as_bytes(), "fastmail.com")).username, None);
    }

    #[test]
    fn invalid_hosts_and_ports_are_invalid() {
        for bad_host in [
            "127.0.0.1",
            "localhost",
            "a b.test",
            "imap_x.test",
            "-a.test",
            "mail",
        ] {
            let doc = FASTMAIL.replace("imap.fastmail.com", bad_host);
            assert_eq!(
                parse(doc.as_bytes(), "fastmail.com"),
                XmlOutcome::Invalid,
                "{bad_host}"
            );
        }
        for bad_port in ["0", "65536", "-1", "abc", ""] {
            let doc = FASTMAIL.replacen("<port>993</port>", &format!("<port>{bad_port}</port>"), 1);
            assert_eq!(
                parse(doc.as_bytes(), "fastmail.com"),
                XmlOutcome::Invalid,
                "port {bad_port:?}"
            );
        }
    }

    #[test]
    fn smtp_security_must_match_the_port_convention() {
        // SSL on 587 would be sent as STARTTLS by the core: not usable.
        let doc = GMAIL.replace("<port>465</port>", "<port>587</port>");
        assert_eq!(found(parse(doc.as_bytes(), "gmail.com")).smtp, None);
    }

    #[test]
    fn pop3_only_is_a_miss_and_garbage_is_invalid() {
        let doc = r#"<clientConfig><emailProvider id="p.test"><incomingServer type="pop3">
            <hostname>pop.p.test</hostname><port>995</port><socketType>SSL</socketType>
            </incomingServer></emailProvider></clientConfig>"#;
        assert_eq!(parse(doc.as_bytes(), "p.test"), XmlOutcome::NoImap);
        for bad in [
            "".as_bytes(),
            b"<html><body>Sign in to the Wi-Fi</body></html>",
            b"<clientConfig><emailProvider",
            b"\xff\xfe\x00",
            b"<clientConfig/>",
        ] {
            assert_eq!(parse(bad, "p.test"), XmlOutcome::Invalid);
        }
    }

    #[test]
    fn dtd_and_entity_bombs_are_refused() {
        let bomb = r#"<?xml version="1.0"?>
<!DOCTYPE lolz [<!ENTITY a "aaaaaaaaaa"><!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;">]>
<clientConfig><emailProvider id="x"><displayName>&b;</displayName></emailProvider></clientConfig>"#;
        assert_eq!(parse(bomb.as_bytes(), "x.test"), XmlOutcome::Invalid);
    }

    #[test]
    fn oversized_body_is_refused() {
        let mut big = GMAIL.to_string();
        big.push_str(&" ".repeat(MAX_BODY));
        assert_eq!(parse(big.as_bytes(), "gmail.com"), XmlOutcome::Invalid);
    }

    #[test]
    fn display_name_is_cleaned_and_capped() {
        let long = "N".repeat(200);
        let doc = FASTMAIL.replace("FastMail", &format!("  A\tB{long}  "));
        let name = found(parse(doc.as_bytes(), "fastmail.com"))
            .display_name
            .unwrap();
        assert!(name.chars().count() <= MAX_DISPLAY_NAME);
        assert!(name.starts_with("AB"));
        assert!(!name.chars().any(|c| c.is_control()));
    }
}
