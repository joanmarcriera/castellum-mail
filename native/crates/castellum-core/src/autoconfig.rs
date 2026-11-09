// Castellum - account-setup auto-detect (MM-14, slice 1;).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Given a mail DOMAIN, find IMAP/SMTP server settings. Sources, tried in this
// fixed order, first usable answer wins (cheapest and least revealing first):
//   1. DNS SRV (RFC 6186)          -- `_imaps._tcp`, `_submissions._tcp`, `_submission._tcp`
//   2. MX hint                     -- a known hosting provider maps to a preset id
//   3. domain autoconfig           -- https://autoconfig.<domain>/mail/config-v1.1.xml
//   4. well-known                  -- https://<domain>/.well-known/autoconfig/mail/config-v1.1.xml
//   5. Mozilla ISPDB               -- https://autoconfig.thunderbird.net/v1.1/<domain>
//
// Privacy and safety invariants (each is a test):
//   * Only the DOMAIN enters this module. The request type has no field for
//     the local part and refuses unknown fields; a value containing `@` is
//     rejected. No password, username or address is ever sent anywhere.
//   * Nothing here connects to the mail servers it returns, and it never sets
//     `allow_invalid_certs`. Results only pre-fill a form the user reviews.
//   * Plaintext IMAP is never returned: IMAP is implicit TLS or STARTTLS.
//   * Whole-lookup deadline (default 5 s, clamped 1..=15 s) and a cooperative
//     cancel flag; the DNS and HTTP transports are injected so unit tests use
//     fixtures and never touch the network.
//
// The blocking entry point for the C ABI is `lookup_json`.

mod dns;
mod xml;

use crate::CoreError;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub use dns::SystemDns;
use xml::XmlOutcome;

const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const MIN_TIMEOUT_MS: u64 = 1_000;
const MAX_TIMEOUT_MS: u64 = 15_000;
const MAX_REDIRECTS: u32 = 3;
const USER_AGENT: &str = "Castellum-autoconfig";

// ---------------------------------------------------------------- wire types

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum AutoconfigSource {
    Srv,
    Mx,
    DomainAutoconfig,
    WellKnown,
    Ispdb,
}

/// The fixed order sources are tried in, whatever order a request lists them.
const SOURCE_ORDER: [AutoconfigSource; 5] = [
    AutoconfigSource::Srv,
    AutoconfigSource::Mx,
    AutoconfigSource::DomainAutoconfig,
    AutoconfigSource::WellKnown,
    AutoconfigSource::Ispdb,
];

fn default_sources() -> Vec<AutoconfigSource> {
    SOURCE_ORDER.to_vec()
}

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

/// A lookup request. `deny_unknown_fields` is the structural guarantee that
/// nothing but a domain (and tuning knobs) can be passed in.
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct AutoconfigRequest {
    /// Lower-case ASCII/punycode domain; validated before any I/O.
    pub domain: String,
    /// Enables/disables sources; the ORDER is fixed by the core.
    #[serde(default = "default_sources")]
    pub sources: Vec<AutoconfigSource>,
    /// Whole-lookup deadline, clamped to 1000..=15000.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum AutoconfigStatus {
    Found,
    Preset,
    Unsupported,
    NotFound,
    Offline,
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    Implicit,
    Starttls,
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum UsernameForm {
    Email,
    LocalPart,
}

#[derive(Serialize, Clone, PartialEq, Eq, Debug)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub tls: TlsMode,
}

/// Wire name kept from the spec (`ServerEndpoint`).
pub type ServerEndpoint = Endpoint;

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    Hit,
    Miss,
    Timeout,
    Invalid,
    Skipped,
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct SourceAttempt {
    pub source: AutoconfigSource,
    pub outcome: AttemptOutcome,
}

#[derive(Serialize, Debug)]
pub struct AutoconfigReport {
    pub status: AutoconfigStatus,
    pub source: Option<AutoconfigSource>,
    /// status = preset: "gmail" | "fastmail" | "icloud" | "yahoo".
    pub preset_id: Option<&'static str>,
    /// status = unsupported: "microsoft" | "oauth_only" | "no_tls_imap".
    pub unsupported: Option<&'static str>,
    pub display_name: Option<String>,
    pub imap: Option<Endpoint>,
    pub smtp: Option<Endpoint>,
    pub username: Option<UsernameForm>,
    /// True when a DNS-sourced server name is not under the looked-up domain.
    /// DNS is unauthenticated, so the Add-account sheet must NOT pre-fill such
    /// a host (credentials would go to whoever the answer names).
    pub host_outside_domain: bool,
    pub tried: Vec<SourceAttempt>,
}

/// Every preset id the MX table can return (a Swift test asserts each one
/// resolves through `ProviderPreset.byId`).
pub const PRESET_IDS: [&str; 4] = ["gmail", "fastmail", "icloud", "yahoo"];

// ---------------------------------------------------------------- transports

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SrvRecord {
    pub priority: u16,
    pub weight: u16,
    pub port: u16,
    /// Lower-cased target; `"."` means "service not offered" (RFC 2782).
    pub target: String,
}

/// A resolver failure that is not "the name has no such records".
#[derive(Debug, PartialEq, Eq)]
pub enum DnsError {
    /// Timeout, SERVFAIL, no network: the resolver could not answer.
    Unavailable,
}

/// DNS seam. `Ok(vec![])` means a definitive "no records" (NXDOMAIN/NODATA).
pub trait DnsTransport: Send + Sync {
    fn srv(&self, name: &str, deadline: Instant) -> Result<Vec<SrvRecord>, DnsError>;
    /// MX exchange hosts, lowest preference first.
    fn mx(&self, domain: &str, deadline: Instant) -> Result<Vec<String>, DnsError>;
}

#[derive(Debug, PartialEq, Eq)]
pub enum FetchError {
    /// The deadline passed.
    Timeout,
    /// Could not resolve or connect (offline, NXDOMAIN, refused).
    Unreachable,
    /// The server answered with a non-success status (404 is the usual one).
    Status(u16),
    /// Body over the cap, too many/forbidden redirects, TLS failure, etc.
    Invalid,
}

/// HTTP seam: a GET that returns at most 64 KiB, follows at most 3 redirects
/// and never leaves HTTPS. Sends no cookies, credentials or address.
pub trait HttpTransport: Send + Sync {
    fn get(&self, url: &str, deadline: Instant) -> Result<Vec<u8>, FetchError>;
}

/// Production HTTP transport (ureq + rustls).
pub struct UreqHttp {
    https_only: bool,
    guard: bool,
}

impl UreqHttp {
    pub fn production() -> Self {
        UreqHttp {
            https_only: true,
            guard: true,
        }
    }

    /// Loopback test servers speak plain HTTP on 127.0.0.1.
    #[cfg(test)]
    fn for_loopback_tests() -> Self {
        UreqHttp {
            https_only: false,
            guard: false,
        }
    }
}

impl HttpTransport for UreqHttp {
    fn get(&self, url: &str, deadline: Instant) -> Result<Vec<u8>, FetchError> {
        use ureq::unversioned::resolver::DefaultResolver;
        use ureq::unversioned::transport::DefaultConnector;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(FetchError::Timeout);
        }
        // `proxy(None)`: never inherit ALL_PROXY from the environment (it
        // would also bypass the address guard). Status errors stay errors.
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(remaining))
            .max_redirects(MAX_REDIRECTS)
            .https_only(self.https_only)
            .proxy(None)
            .user_agent(USER_AGENT)
            .build();
        // Every hop (first request and each redirect) is checked against the
        // public-address allowlist, as for the one-click unsubscribe POST:
        // a hostile domain cannot aim this GET at the LAN or loopback.
        let allow: Box<dyn Fn(&std::net::SocketAddr) -> bool + Send + Sync> = if self.guard {
            Box::new(crate::unsubscribe::is_public_target)
        } else {
            Box::new(|_| true)
        };
        let agent = ureq::Agent::with_parts(
            config,
            DefaultConnector::new(),
            crate::unsubscribe::GuardedResolver {
                inner: DefaultResolver::default(),
                allow,
            },
        );
        let mut resp = agent.get(url).call().map_err(map_ureq_error)?;
        resp.body_mut()
            .with_config()
            .limit(xml::MAX_BODY as u64)
            .read_to_vec()
            .map_err(map_ureq_error)
    }
}

fn map_ureq_error(e: ureq::Error) -> FetchError {
    match e {
        ureq::Error::Timeout(_) => FetchError::Timeout,
        ureq::Error::StatusCode(c) => FetchError::Status(c),
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed | ureq::Error::Io(_) => {
            FetchError::Unreachable
        }
        _ => FetchError::Invalid,
    }
}

// ------------------------------------------------------------ fuzz entry points

/// Decode arbitrary bytes as a DNS response (fuzz target
/// `dns_answer_decode`). The property under test: never panics.
#[doc(hidden)]
pub fn fuzz_dns_answer(data: &[u8]) -> usize {
    let a = dns::parse_answer(data, "example.org");
    a.srv.len() + a.mx.len()
}

/// Parse arbitrary bytes as an autoconfig document (fuzz target
/// `autoconfig_xml_decode`). The property under test: never panics, and a
/// `Found` result always carries a valid host and a non-zero port.
#[doc(hidden)]
pub fn fuzz_autoconfig_xml(data: &[u8]) -> bool {
    match xml::parse(data, "example.org") {
        XmlOutcome::Found(f) => xml::valid_host(&f.imap.host) && f.imap.port != 0,
        _ => false,
    }
}

// ------------------------------------------------------------------- lookup

/// A fully resolved answer from one source.
#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct Found {
    pub display_name: Option<String>,
    pub imap: Endpoint,
    pub smtp: Option<Endpoint>,
    pub username: Option<UsernameForm>,
    /// A DNS (SRV) host outside the looked-up domain; see the report field.
    pub outside_domain: bool,
}

enum Resolution {
    Found(Found),
    Preset(&'static str),
    Unsupported(&'static str),
}

enum Outcome {
    Hit(Resolution),
    Miss,
    Timeout,
    Invalid,
}

/// The core's SMTP sender uses implicit TLS iff the port is 465 and mandatory
/// STARTTLS otherwise, so an endpoint is only representable when it agrees.
pub(crate) fn smtp_consistent(ep: &Endpoint) -> bool {
    match ep.tls {
        TlsMode::Implicit => ep.port == 465,
        TlsMode::Starttls => ep.port != 465,
    }
}

/// MX host suffix -> provider. Hosts live in Swift's `ProviderPreset`; this
/// returns only an id (or "unsupported microsoft").
const MX_SUFFIXES: &[(&str, MxKind)] = &[
    ("google.com", MxKind::Preset("gmail")),
    ("googlemail.com", MxKind::Preset("gmail")),
    ("messagingengine.com", MxKind::Preset("fastmail")),
    ("icloud.com", MxKind::Preset("icloud")),
    ("yahoodns.net", MxKind::Preset("yahoo")),
    ("mail.protection.outlook.com", MxKind::Microsoft),
    ("olc.protection.outlook.com", MxKind::Microsoft),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MxKind {
    Preset(&'static str),
    Microsoft,
}

fn host_has_suffix(host: &str, suffix: &str) -> bool {
    host == suffix
        || host
            .strip_suffix(suffix)
            .is_some_and(|rest| rest.ends_with('.'))
}

/// Validate a request domain: lower-case ASCII/punycode, 2+ labels, final
/// label alphabetic (or `xn--`) and >= 2 chars. Rejects `@` and anything else.
pub fn normalize_domain(raw: &str) -> Option<String> {
    let d = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    if !xml::valid_host(&d) {
        return None;
    }
    let last = d.rsplit('.').next()?;
    // Reserved names (RFC 2606 / 6761) can never have public autoconfig data;
    // refusing them keeps tests and fixtures from ever reaching the network.
    if matches!(last, "test" | "example" | "invalid" | "localhost" | "local") {
        return None;
    }
    let tld_ok = last.len() >= 2
        && (last.starts_with("xn--") || last.bytes().all(|c| c.is_ascii_lowercase()));
    tld_ok.then_some(d)
}

fn finish(
    status: AutoconfigStatus,
    source: Option<AutoconfigSource>,
    res: Option<Resolution>,
    tried: Vec<SourceAttempt>,
) -> AutoconfigReport {
    let mut r = AutoconfigReport {
        status,
        source,
        preset_id: None,
        unsupported: None,
        display_name: None,
        imap: None,
        smtp: None,
        username: None,
        host_outside_domain: false,
        tried,
    };
    match res {
        Some(Resolution::Found(f)) => {
            r.display_name = f.display_name;
            r.imap = Some(f.imap);
            r.smtp = f.smtp;
            r.username = f.username;
            r.host_outside_domain = f.outside_domain;
        }
        Some(Resolution::Preset(id)) => r.preset_id = Some(id),
        Some(Resolution::Unsupported(why)) => r.unsupported = Some(why),
        None => {}
    }
    r
}

/// Run a lookup with no cancellation. See [`lookup_cancellable`].
pub fn lookup(
    req: &AutoconfigRequest,
    dns: &dyn DnsTransport,
    http: &dyn HttpTransport,
) -> AutoconfigReport {
    lookup_cancellable(req, dns, http, &AtomicBool::new(false))
}

/// Run the lookup. `cancel` is checked between sources (and the transports
/// are bounded by the same deadline), so a caller that raises it gets control
/// back after at most the in-flight request. The report for a request with an
/// invalid domain is `not_found` with no attempts and no I/O.
pub fn lookup_cancellable(
    req: &AutoconfigRequest,
    dns: &dyn DnsTransport,
    http: &dyn HttpTransport,
    cancel: &AtomicBool,
) -> AutoconfigReport {
    let Some(domain) = normalize_domain(&req.domain) else {
        return finish(AutoconfigStatus::NotFound, None, None, Vec::new());
    };
    let budget = req.timeout_ms.clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS);
    let deadline = Instant::now() + Duration::from_millis(budget);
    let mut tried: Vec<SourceAttempt> = Vec::new();
    let mut mx_hosts: Vec<String> = Vec::new();
    // An off-domain SRV answer is only a last resort (see `Found::outside_domain`).
    let mut deferred_srv: Option<Found> = None;

    for source in SOURCE_ORDER {
        if !req.sources.contains(&source) {
            continue;
        }
        if cancel.load(Ordering::Relaxed) {
            tried.push(SourceAttempt {
                source,
                outcome: AttemptOutcome::Skipped,
            });
            continue;
        }
        let outcome = if Instant::now() >= deadline {
            Outcome::Timeout
        } else {
            match source {
                AutoconfigSource::Srv => try_srv(&domain, dns, deadline),
                AutoconfigSource::Mx => try_mx(&domain, dns, deadline, &mut mx_hosts),
                AutoconfigSource::DomainAutoconfig => try_xml(
                    &format!("https://autoconfig.{domain}/mail/config-v1.1.xml"),
                    &domain,
                    http,
                    deadline,
                ),
                AutoconfigSource::WellKnown => try_xml(
                    &format!("https://{domain}/.well-known/autoconfig/mail/config-v1.1.xml"),
                    &domain,
                    http,
                    deadline,
                ),
                AutoconfigSource::Ispdb => try_ispdb(&domain, &mx_hosts, http, deadline),
            }
        };
        let attempt = |o| SourceAttempt { source, outcome: o };
        match outcome {
            Outcome::Hit(Resolution::Found(f))
                if source == AutoconfigSource::Srv && f.outside_domain =>
            {
                // Prefer the HTTPS sources over an unauthenticated off-domain
                // DNS answer: keep it aside and carry on.
                tried.push(attempt(AttemptOutcome::Hit));
                deferred_srv = Some(f);
            }
            Outcome::Hit(res) => {
                tried.push(attempt(AttemptOutcome::Hit));
                let status = match res {
                    Resolution::Found(_) => AutoconfigStatus::Found,
                    Resolution::Preset(_) => AutoconfigStatus::Preset,
                    Resolution::Unsupported(_) => AutoconfigStatus::Unsupported,
                };
                return finish(status, Some(source), Some(res), tried);
            }
            Outcome::Miss => tried.push(attempt(AttemptOutcome::Miss)),
            Outcome::Timeout => tried.push(attempt(AttemptOutcome::Timeout)),
            Outcome::Invalid => tried.push(attempt(AttemptOutcome::Invalid)),
        }
    }

    if let Some(f) = deferred_srv {
        return finish(
            AutoconfigStatus::Found,
            Some(AutoconfigSource::Srv),
            Some(Resolution::Found(f)),
            tried,
        );
    }

    // Offline: something was attempted and every attempt failed to get an
    // answer. Any definitive miss (or invalid answer) means we did reach the
    // network, so the domain simply has nothing.
    let attempted: Vec<_> = tried
        .iter()
        .filter(|a| a.outcome != AttemptOutcome::Skipped)
        .collect();
    let offline = !attempted.is_empty()
        && attempted
            .iter()
            .all(|a| a.outcome == AttemptOutcome::Timeout);
    let status = if offline {
        AutoconfigStatus::Offline
    } else {
        AutoconfigStatus::NotFound
    };
    finish(status, None, None, tried)
}

/// Lowest priority first, then highest weight; ties keep answer order.
fn sorted_srv(mut recs: Vec<SrvRecord>) -> Vec<SrvRecord> {
    recs.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then_with(|| b.weight.cmp(&a.weight))
    });
    recs.retain(|r| r.target != ".");
    recs
}

/// What the best SRV record for a name amounts to.
enum SrvPick {
    /// No usable record (none, or only the RFC 2782 "." target).
    Miss,
    /// The best record names an invalid host or port.
    Invalid,
    Hit(String, u16),
}

/// Best SRV endpoint for `name`; `Err` is an unavailable resolver.
fn srv_best(dns: &dyn DnsTransport, name: &str, deadline: Instant) -> Result<SrvPick, DnsError> {
    let recs = sorted_srv(dns.srv(name, deadline)?);
    Ok(match recs.into_iter().next() {
        None => SrvPick::Miss,
        Some(r) => {
            let host = r.target.trim_end_matches('.').to_string();
            if xml::valid_host(&host) && r.port != 0 {
                SrvPick::Hit(host, r.port)
            } else {
                SrvPick::Invalid
            }
        }
    })
}

fn try_srv(domain: &str, dns: &dyn DnsTransport, deadline: Instant) -> Outcome {
    // IMAP: implicit TLS only; `_imap._tcp` (STARTTLS/cleartext) is ignored.
    let imap = match srv_best(dns, &format!("_imaps._tcp.{domain}"), deadline) {
        Err(DnsError::Unavailable) => return Outcome::Timeout,
        Ok(SrvPick::Miss) => return Outcome::Miss,
        Ok(SrvPick::Invalid) => return Outcome::Invalid,
        Ok(SrvPick::Hit(host, port)) => Endpoint {
            host,
            port,
            tls: TlsMode::Implicit,
        },
    };
    // SMTP: submissions (implicit TLS) first, then submission (STARTTLS). SMTP
    // alone is never a hit, and a missing/odd SMTP record just leaves it blank.
    let mut smtp = None;
    for (label, tls) in [
        ("_submissions", TlsMode::Implicit),
        ("_submission", TlsMode::Starttls),
    ] {
        if let Ok(SrvPick::Hit(host, port)) =
            srv_best(dns, &format!("{label}._tcp.{domain}"), deadline)
        {
            let ep = Endpoint { host, port, tls };
            if smtp_consistent(&ep) {
                smtp = Some(ep);
                break;
            }
        }
    }
    let outside_domain = !host_has_suffix(&imap.host, domain)
        || smtp
            .as_ref()
            .is_some_and(|s| !host_has_suffix(&s.host, domain));
    Outcome::Hit(Resolution::Found(Found {
        display_name: None,
        imap,
        smtp,
        username: None,
        outside_domain,
    }))
}

fn try_mx(
    domain: &str,
    dns: &dyn DnsTransport,
    deadline: Instant,
    mx_hosts: &mut Vec<String>,
) -> Outcome {
    let hosts = match dns.mx(domain, deadline) {
        Err(DnsError::Unavailable) => return Outcome::Timeout,
        Ok(h) => h,
    };
    *mx_hosts = hosts
        .iter()
        .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
        .filter(|h| !h.is_empty() && h != ".")
        .collect();
    let mut kind: Option<MxKind> = None;
    for host in mx_hosts.iter() {
        let this = MX_SUFFIXES
            .iter()
            .find(|(s, _)| host_has_suffix(host, s))
            .map(|(_, k)| *k);
        match (this, kind) {
            (None, _) => return Outcome::Miss,
            (Some(t), None) => kind = Some(t),
            (Some(t), Some(k)) if t == k => {}
            _ => return Outcome::Miss, // MX hosts disagree
        }
    }
    match kind {
        Some(MxKind::Preset(id)) => Outcome::Hit(Resolution::Preset(id)),
        Some(MxKind::Microsoft) => Outcome::Hit(Resolution::Unsupported("microsoft")),
        None => Outcome::Miss,
    }
}

fn xml_to_outcome(x: XmlOutcome) -> Outcome {
    match x {
        XmlOutcome::Found(f) => Outcome::Hit(Resolution::Found(f)),
        XmlOutcome::OAuthOnly => Outcome::Hit(Resolution::Unsupported("oauth_only")),
        XmlOutcome::NoTlsImap => Outcome::Hit(Resolution::Unsupported("no_tls_imap")),
        XmlOutcome::NoImap => Outcome::Miss,
        XmlOutcome::Invalid => Outcome::Invalid,
    }
}

/// Fetch one URL and read it as an autoconfig document.
fn fetch_xml(url: &str, domain: &str, http: &dyn HttpTransport, deadline: Instant) -> Outcome {
    match http.get(url, deadline) {
        Ok(body) => xml_to_outcome(xml::parse(&body, domain)),
        Err(FetchError::Timeout) | Err(FetchError::Unreachable) => Outcome::Timeout,
        Err(FetchError::Status(_)) => Outcome::Miss,
        Err(FetchError::Invalid) => Outcome::Invalid,
    }
}

fn try_xml(url: &str, domain: &str, http: &dyn HttpTransport, deadline: Instant) -> Outcome {
    fetch_xml(url, domain, http, deadline)
}

/// The "base" of an MX host for Thunderbird's second ISPDB try: its last two
/// labels, or three when the second-to-last is a short generic second-level
/// label under a country code (`mx.mail.example.co.uk` -> `example.co.uk`).
/// There is no public-suffix list here; a wrong guess costs one useless query
/// of a name derived from DNS data, never anything about the user.
fn mx_base_domain(host: &str) -> Option<String> {
    let labels: Vec<&str> = host.split('.').collect();
    let n = labels.len();
    if n < 2 {
        return None;
    }
    let tld = labels[n - 1];
    let sld = labels[n - 2];
    let take = if n >= 3
        && tld.len() == 2
        && matches!(
            sld,
            "co" | "com" | "org" | "net" | "ac" | "gov" | "edu" | "or" | "ne"
        ) {
        3
    } else {
        2
    };
    Some(labels[n - take..].join("."))
}

fn try_ispdb(
    domain: &str,
    mx_hosts: &[String],
    http: &dyn HttpTransport,
    deadline: Instant,
) -> Outcome {
    let first = fetch_xml(
        &format!("https://autoconfig.thunderbird.net/v1.1/{domain}"),
        domain,
        http,
        deadline,
    );
    if !matches!(first, Outcome::Miss) {
        return first;
    }
    // Thunderbird's second try: the MX host's base domain, once.
    let base = mx_hosts.first().and_then(|h| mx_base_domain(h));
    match base {
        Some(b) if b != domain && Instant::now() < deadline => {
            let second = fetch_xml(
                &format!("https://autoconfig.thunderbird.net/v1.1/{b}"),
                domain,
                http,
                deadline,
            );
            // The key came from unauthenticated MX data, not from the user's
            // domain: flag the result unless its hosts sit under that domain.
            match second {
                Outcome::Hit(Resolution::Found(mut f)) => {
                    let under = host_has_suffix(&f.imap.host, domain)
                        && f.smtp
                            .as_ref()
                            .is_none_or(|s| host_has_suffix(&s.host, domain));
                    f.outside_domain = f.outside_domain || !under;
                    Outcome::Hit(Resolution::Found(f))
                }
                other => other,
            }
        }
        _ => first,
    }
}

/// Production entry point for the C ABI: parse `request_json`, run the lookup
/// with the system resolver and rustls HTTP, and return the report JSON.
/// Only a malformed request is an error; "nothing found" and "offline" are
/// reported inside the report.
pub fn lookup_json(request_json: &str) -> Result<String, CoreError> {
    let req: AutoconfigRequest =
        serde_json::from_str(request_json).map_err(|_| CoreError::InvalidArgument)?;
    if normalize_domain(&req.domain).is_none() {
        return Err(CoreError::InvalidArgument);
    }
    let report = lookup(&req, &SystemDns, &UreqHttp::production());
    serde_json::to_string(&report).map_err(|_| CoreError::InvalidArgument)
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use xml::fixtures::*;

    /// Fake DNS: canned answers per query name; records every query.
    #[derive(Default)]
    struct FakeDns {
        srv: HashMap<String, Result<Vec<SrvRecord>, ()>>,
        mx: Option<Result<Vec<String>, ()>>,
        log: Mutex<Vec<String>>,
    }

    impl FakeDns {
        fn with_srv(mut self, name: &str, recs: Vec<(u16, u16, u16, &str)>) -> Self {
            self.srv.insert(
                name.to_string(),
                Ok(recs
                    .into_iter()
                    .map(|(priority, weight, port, t)| SrvRecord {
                        priority,
                        weight,
                        port,
                        target: t.to_string(),
                    })
                    .collect()),
            );
            self
        }
        fn with_mx(mut self, hosts: &[&str]) -> Self {
            self.mx = Some(Ok(hosts.iter().map(|h| h.to_string()).collect()));
            self
        }
        fn down() -> Self {
            FakeDns {
                mx: Some(Err(())),
                srv: ["_imaps._tcp.example.org"]
                    .iter()
                    .map(|n| (n.to_string(), Err(())))
                    .collect(),
                ..Default::default()
            }
        }
    }

    impl DnsTransport for FakeDns {
        fn srv(&self, name: &str, _d: Instant) -> Result<Vec<SrvRecord>, DnsError> {
            self.log.lock().unwrap().push(format!("srv {name}"));
            match self.srv.get(name) {
                Some(Ok(r)) => Ok(r.clone()),
                Some(Err(())) => Err(DnsError::Unavailable),
                None => Ok(Vec::new()),
            }
        }
        fn mx(&self, domain: &str, _d: Instant) -> Result<Vec<String>, DnsError> {
            self.log.lock().unwrap().push(format!("mx {domain}"));
            match &self.mx {
                Some(Ok(h)) => Ok(h.clone()),
                Some(Err(())) => Err(DnsError::Unavailable),
                None => Ok(Vec::new()),
            }
        }
    }

    /// Fake HTTP: canned answers per URL (default 404); records every URL.
    #[derive(Default)]
    struct FakeHttp {
        map: HashMap<String, Result<Vec<u8>, FetchErr>>,
        log: Mutex<Vec<String>>,
        default_err: Option<FetchErr>,
    }

    #[derive(Clone, Copy)]
    enum FetchErr {
        Timeout,
        Unreachable,
        Invalid,
    }

    impl FakeHttp {
        fn with(mut self, url: &str, body: &str) -> Self {
            self.map.insert(url.into(), Ok(body.as_bytes().to_vec()));
            self
        }
        fn with_err(mut self, url: &str, e: FetchErr) -> Self {
            self.map.insert(url.into(), Err(e));
            self
        }
        fn failing(e: FetchErr) -> Self {
            FakeHttp {
                default_err: Some(e),
                ..Default::default()
            }
        }
        fn urls(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    impl HttpTransport for FakeHttp {
        fn get(&self, url: &str, _d: Instant) -> Result<Vec<u8>, FetchError> {
            self.log.lock().unwrap().push(url.to_string());
            let conv = |e: FetchErr| match e {
                FetchErr::Timeout => FetchError::Timeout,
                FetchErr::Unreachable => FetchError::Unreachable,
                FetchErr::Invalid => FetchError::Invalid,
            };
            match self.map.get(url) {
                Some(Ok(b)) => Ok(b.clone()),
                Some(Err(e)) => Err(conv(*e)),
                None => match self.default_err {
                    Some(e) => Err(conv(e)),
                    None => Err(FetchError::Status(404)),
                },
            }
        }
    }

    fn req(domain: &str) -> AutoconfigRequest {
        AutoconfigRequest {
            domain: domain.into(),
            sources: default_sources(),
            timeout_ms: 5_000,
        }
    }

    const D: &str = "example.org";
    const AUTOCONFIG: &str = "https://autoconfig.example.org/mail/config-v1.1.xml";
    const WELL_KNOWN: &str = "https://example.org/.well-known/autoconfig/mail/config-v1.1.xml";
    const ISPDB: &str = "https://autoconfig.thunderbird.net/v1.1/example.org";

    fn outcomes(r: &AutoconfigReport) -> Vec<(AutoconfigSource, AttemptOutcome)> {
        r.tried.iter().map(|a| (a.source, a.outcome)).collect()
    }

    // ---- SRV

    #[test]
    fn srv_orders_by_priority_then_weight() {
        let dns = FakeDns::default().with_srv(
            "_imaps._tcp.example.org",
            vec![
                (20, 100, 993, "low-prio.example.org"),
                (10, 5, 993, "light.example.org"),
                (10, 50, 993, "heavy.example.org"),
            ],
        );
        let r = lookup(&req(D), &dns, &FakeHttp::default());
        assert_eq!(r.status, AutoconfigStatus::Found);
        assert_eq!(r.source, Some(AutoconfigSource::Srv));
        assert_eq!(r.imap.unwrap().host, "heavy.example.org");
    }

    #[test]
    fn srv_dot_target_means_not_offered() {
        let dns = FakeDns::default().with_srv("_imaps._tcp.example.org", vec![(0, 0, 0, ".")]);
        let r = lookup(&req(D), &dns, &FakeHttp::default());
        assert_eq!(r.status, AutoconfigStatus::NotFound);
        assert_eq!(
            outcomes(&r)[0],
            (AutoconfigSource::Srv, AttemptOutcome::Miss)
        );
    }

    #[test]
    fn srv_smtp_alone_is_a_miss_and_cleartext_imap_srv_is_ignored() {
        let dns = FakeDns::default()
            .with_srv(
                "_submission._tcp.example.org",
                vec![(0, 0, 587, "smtp.example.org")],
            )
            .with_srv(
                "_imap._tcp.example.org",
                vec![(0, 0, 143, "imap.example.org")],
            );
        let r = lookup(&req(D), &dns, &FakeHttp::default());
        assert_eq!(r.status, AutoconfigStatus::NotFound);
        assert!(!dns
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|q| q.contains("_imap._tcp")));
    }

    #[test]
    fn srv_in_domain_target_is_unflagged() {
        let dns = FakeDns::default().with_srv(
            "_imaps._tcp.example.org",
            vec![(0, 0, 993, "imap.example.org")],
        );
        let r = lookup(&req(D), &dns, &FakeHttp::default());
        assert_eq!(r.status, AutoconfigStatus::Found);
        assert!(!r.host_outside_domain);
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["host_outside_domain"], false);
    }

    #[test]
    fn srv_off_domain_target_is_flagged_but_https_sources_win() {
        // SECURITY: unauthenticated DNS must not beat an HTTPS answer.
        let dns = FakeDns::default().with_srv(
            "_imaps._tcp.example.org",
            vec![(0, 0, 993, "imap.hoster.test")],
        );
        let http = FakeHttp::default().with(WELL_KNOWN, FASTMAIL);
        let r = lookup(&req(D), &dns, &http);
        assert_eq!(r.source, Some(AutoconfigSource::WellKnown));
        assert!(!r.host_outside_domain);
        assert_eq!(r.imap.unwrap().host, "imap.fastmail.com");
    }

    #[test]
    fn srv_off_domain_target_is_a_flagged_last_resort() {
        let dns = FakeDns::default()
            .with_srv(
                "_imaps._tcp.example.org",
                vec![(0, 0, 993, "imap.hoster.test")],
            )
            .with_srv(
                "_submission._tcp.example.org",
                vec![(0, 0, 587, "smtp.example.org")],
            );
        let r = lookup(&req(D), &dns, &FakeHttp::default());
        assert_eq!(r.status, AutoconfigStatus::Found);
        assert_eq!(r.source, Some(AutoconfigSource::Srv));
        assert!(r.host_outside_domain, "the UI must not pre-fill this");
        // A look-alike suffix is still outside the domain.
        let dns = FakeDns::default().with_srv(
            "_imaps._tcp.example.org",
            vec![(0, 0, 993, "imap.evilexample.org")],
        );
        assert!(lookup(&req(D), &dns, &FakeHttp::default()).host_outside_domain);
        // An off-domain SMTP host alone flags it too (credentials go there).
        let dns = FakeDns::default()
            .with_srv(
                "_imaps._tcp.example.org",
                vec![(0, 0, 993, "imap.example.org")],
            )
            .with_srv(
                "_submission._tcp.example.org",
                vec![(0, 0, 587, "smtp.hoster.test")],
            );
        assert!(lookup(&req(D), &dns, &FakeHttp::default()).host_outside_domain);
    }

    #[test]
    fn srv_hit_fills_smtp_submissions_before_submission() {
        let dns = FakeDns::default()
            .with_srv(
                "_imaps._tcp.example.org",
                vec![(0, 1, 993, "imap.example.org.")],
            )
            .with_srv(
                "_submissions._tcp.example.org",
                vec![(0, 1, 465, "smtp.example.org")],
            )
            .with_srv(
                "_submission._tcp.example.org",
                vec![(0, 1, 587, "other.example.org")],
            );
        let r = lookup(&req(D), &dns, &FakeHttp::default());
        let imap = r.imap.unwrap();
        assert_eq!(
            (imap.host.as_str(), imap.port, imap.tls),
            ("imap.example.org", 993, TlsMode::Implicit)
        );
        let smtp = r.smtp.unwrap();
        assert_eq!(
            (smtp.host.as_str(), smtp.tls),
            ("smtp.example.org", TlsMode::Implicit)
        );
    }

    #[test]
    fn srv_submission_starttls_is_used_when_no_submissions() {
        let dns = FakeDns::default()
            .with_srv(
                "_imaps._tcp.example.org",
                vec![(0, 1, 993, "imap.example.org")],
            )
            .with_srv(
                "_submission._tcp.example.org",
                vec![(0, 1, 587, "smtp.example.org")],
            );
        let r = lookup(&req(D), &dns, &FakeHttp::default());
        assert_eq!(r.smtp.unwrap().tls, TlsMode::Starttls);
    }

    #[test]
    fn srv_with_a_bad_host_or_port_is_invalid_and_lookup_continues() {
        let dns =
            FakeDns::default().with_srv("_imaps._tcp.example.org", vec![(0, 0, 993, "127.0.0.1")]);
        let http = FakeHttp::default().with(ISPDB, FASTMAIL);
        let r = lookup(&req(D), &dns, &http);
        assert_eq!(
            outcomes(&r)[0],
            (AutoconfigSource::Srv, AttemptOutcome::Invalid)
        );
        assert_eq!(r.source, Some(AutoconfigSource::Ispdb));
        let dns = FakeDns::default().with_srv(
            "_imaps._tcp.example.org",
            vec![(0, 0, 0, "imap.example.org")],
        );
        let r = lookup(&req(D), &dns, &FakeHttp::default());
        assert_eq!(outcomes(&r)[0].1, AttemptOutcome::Invalid);
    }

    // ---- MX

    #[test]
    fn mx_suffix_table_maps_providers_to_preset_ids() {
        for (hosts, id) in [
            (
                vec!["aspmx.l.google.com", "alt1.aspmx.l.google.com."],
                "gmail",
            ),
            (vec!["smtp.googlemail.com"], "gmail"),
            (
                vec![
                    "in1-smtp.messagingengine.com",
                    "in2-smtp.messagingengine.com",
                ],
                "fastmail",
            ),
            (vec!["mx01.mail.icloud.com"], "icloud"),
            (vec!["mta5.am0.yahoodns.net"], "yahoo"),
        ] {
            let dns = FakeDns::default().with_mx(&hosts);
            let r = lookup(&req(D), &dns, &FakeHttp::default());
            assert_eq!(r.status, AutoconfigStatus::Preset, "{hosts:?}");
            assert_eq!(r.preset_id, Some(id));
            assert_eq!(r.source, Some(AutoconfigSource::Mx));
            assert!(PRESET_IDS.contains(&id));
        }
    }

    #[test]
    fn mx_microsoft_is_unsupported_and_fills_nothing() {
        let dns = FakeDns::default().with_mx(&["example-org.mail.protection.outlook.com"]);
        let r = lookup(&req(D), &dns, &FakeHttp::default());
        assert_eq!(r.status, AutoconfigStatus::Unsupported);
        assert_eq!(r.unsupported, Some("microsoft"));
        assert!(r.imap.is_none() && r.preset_id.is_none());
    }

    #[test]
    fn mx_disagreeing_or_unknown_hosts_are_a_miss() {
        for hosts in [
            vec!["aspmx.l.google.com", "in1-smtp.messagingengine.com"],
            vec!["aspmx.l.google.com", "mail.selfhosted.example.org"],
            vec!["notgoogle.com"],
            vec!["evilgoogle.com"],
        ] {
            let dns = FakeDns::default().with_mx(&hosts);
            let r = lookup(&req(D), &dns, &FakeHttp::default());
            assert_eq!(r.status, AutoconfigStatus::NotFound, "{hosts:?}");
        }
    }

    // ---- domain autoconfig / well-known / ISPDB

    #[test]
    fn domain_autoconfig_then_well_known_then_ispdb_in_order() {
        let http = FakeHttp::default().with(WELL_KNOWN, FASTMAIL);
        let r = lookup(&req(D), &FakeDns::default(), &http);
        assert_eq!(r.source, Some(AutoconfigSource::WellKnown));
        assert_eq!(http.urls(), vec![AUTOCONFIG, WELL_KNOWN]);
        let http = FakeHttp::default().with(ISPDB, GMAIL);
        let r = lookup(&req(D), &FakeDns::default(), &http);
        assert_eq!(r.source, Some(AutoconfigSource::Ispdb));
        assert_eq!(http.urls(), vec![AUTOCONFIG, WELL_KNOWN, ISPDB]);
    }

    #[test]
    fn urls_carry_only_the_domain() {
        let http = FakeHttp::default();
        let _ = lookup(&req(D), &FakeDns::default(), &http);
        for u in http.urls() {
            assert!(
                !u.contains('?') && !u.contains('@') && !u.contains("emailaddress"),
                "{u}"
            );
        }
    }

    #[test]
    fn ispdb_fixtures_for_each_shape() {
        let run = |doc: &str| {
            let http = FakeHttp::default().with(ISPDB, doc);
            lookup(&req(D), &FakeDns::default(), &http)
        };
        let g = run(GMAIL);
        assert_eq!(g.status, AutoconfigStatus::Found);
        assert_eq!(g.imap.unwrap().host, "imap.gmail.com");
        let f = run(FASTMAIL);
        assert_eq!(f.smtp.unwrap().tls, TlsMode::Starttls);
        let o = run(OAUTH_ONLY);
        assert_eq!(
            (o.status, o.unsupported),
            (AutoconfigStatus::Unsupported, Some("oauth_only"))
        );
        assert!(o.imap.is_none());
        let p = run(PLAIN_IMAP_ONLY);
        assert_eq!(
            (p.status, p.unsupported),
            (AutoconfigStatus::Unsupported, Some("no_tls_imap"))
        );
        let s = run(STARTTLS_PLACEHOLDER);
        assert_eq!(s.imap.unwrap().host, "imap.example.org");
    }

    #[test]
    fn malformed_xml_is_invalid_and_not_found() {
        let http = FakeHttp::default().with(ISPDB, "<clientConfig><emailProvider");
        let r = lookup(&req(D), &FakeDns::default(), &http);
        assert_eq!(r.status, AutoconfigStatus::NotFound);
        assert_eq!(outcomes(&r).last().unwrap().1, AttemptOutcome::Invalid);
    }

    #[test]
    fn captive_portal_html_is_a_miss_not_a_crash() {
        let html = "<html><body>Please sign in to the Wi-Fi</body></html>";
        let http = FakeHttp::default()
            .with(AUTOCONFIG, html)
            .with(WELL_KNOWN, html)
            .with(ISPDB, html);
        let r = lookup(&req(D), &FakeDns::default(), &http);
        assert_eq!(r.status, AutoconfigStatus::NotFound);
    }

    #[test]
    fn oversized_body_is_invalid() {
        let http = FakeHttp::default().with_err(AUTOCONFIG, FetchErr::Invalid);
        let r = lookup(&req(D), &FakeDns::default(), &http);
        assert_eq!(
            outcomes(&r)[2],
            (AutoconfigSource::DomainAutoconfig, AttemptOutcome::Invalid)
        );
        let big = format!("{GMAIL}{}", " ".repeat(70 * 1024));
        let http = FakeHttp::default().with(AUTOCONFIG, &big);
        let r = lookup(&req(D), &FakeDns::default(), &http);
        assert_eq!(outcomes(&r)[2].1, AttemptOutcome::Invalid);
    }

    #[test]
    fn ispdb_retries_once_with_the_mx_base_domain() {
        let dns = FakeDns::default().with_mx(&["mx1.mail.hoster.example.net"]);
        let http = FakeHttp::default().with(
            "https://autoconfig.thunderbird.net/v1.1/example.net",
            FASTMAIL,
        );
        let r = lookup(&req(D), &dns, &http);
        assert_eq!(r.source, Some(AutoconfigSource::Ispdb));
        assert!(
            r.host_outside_domain,
            "MX-derived ISPDB key => never auto-filled"
        );
        let urls = http.urls();
        assert_eq!(urls[urls.len() - 2], ISPDB);
        assert_eq!(
            urls[urls.len() - 1],
            "https://autoconfig.thunderbird.net/v1.1/example.net"
        );
        assert_eq!(
            mx_base_domain("mx.mail.example.co.uk").as_deref(),
            Some("example.co.uk")
        );
        assert_eq!(
            mx_base_domain("mx.example.com").as_deref(),
            Some("example.com")
        );
    }

    // ---- ordering, offline, deadline, cancel, request validation

    #[test]
    fn first_hit_wins_and_later_sources_are_not_contacted() {
        let dns = FakeDns::default().with_srv(
            "_imaps._tcp.example.org",
            vec![(0, 0, 993, "imap.example.org")],
        );
        let http = FakeHttp::default().with(ISPDB, GMAIL);
        let r = lookup(&req(D), &dns, &http);
        assert_eq!(r.source, Some(AutoconfigSource::Srv));
        assert!(http.urls().is_empty());
        assert_eq!(r.tried.len(), 1);
    }

    #[test]
    fn source_order_is_fixed_whatever_the_request_says() {
        let mut rq = req(D);
        rq.sources = vec![
            AutoconfigSource::Ispdb,
            AutoconfigSource::WellKnown,
            AutoconfigSource::Mx,
            AutoconfigSource::Srv,
        ];
        let r = lookup(&rq, &FakeDns::default(), &FakeHttp::default());
        let order: Vec<_> = r.tried.iter().map(|a| a.source).collect();
        assert_eq!(
            order,
            vec![
                AutoconfigSource::Srv,
                AutoconfigSource::Mx,
                AutoconfigSource::WellKnown,
                AutoconfigSource::Ispdb
            ]
        );
    }

    #[test]
    fn disabled_sources_are_not_contacted() {
        let mut rq = req(D);
        rq.sources = vec![AutoconfigSource::Srv, AutoconfigSource::Mx];
        let http = FakeHttp::default().with(ISPDB, GMAIL);
        let r = lookup(&rq, &FakeDns::default(), &http);
        assert!(http.urls().is_empty());
        assert_eq!(r.status, AutoconfigStatus::NotFound);
    }

    #[test]
    fn every_source_unavailable_is_offline() {
        let r = lookup(
            &req(D),
            &FakeDns::down(),
            &FakeHttp::failing(FetchErr::Unreachable),
        );
        assert_eq!(r.status, AutoconfigStatus::Offline);
        let r = lookup(
            &req(D),
            &FakeDns::down(),
            &FakeHttp::failing(FetchErr::Timeout),
        );
        assert_eq!(r.status, AutoconfigStatus::Offline);
        assert!(r.tried.iter().all(|a| a.outcome == AttemptOutcome::Timeout));
    }

    #[test]
    fn one_definitive_miss_makes_it_not_found_not_offline() {
        // DNS is down but ISPDB answered 404: we are online, the domain is just unknown.
        let http = FakeHttp::failing(FetchErr::Unreachable);
        let mut http = http;
        http.map.insert(ISPDB.into(), Err(FetchErr::Invalid));
        let r = lookup(&req(D), &FakeDns::down(), &http);
        assert_eq!(r.status, AutoconfigStatus::NotFound);
    }

    #[test]
    fn an_exhausted_budget_stops_contacting_anyone() {
        struct SlowDns;
        impl DnsTransport for SlowDns {
            fn srv(&self, _n: &str, d: Instant) -> Result<Vec<SrvRecord>, DnsError> {
                std::thread::sleep(d.saturating_duration_since(Instant::now()));
                Err(DnsError::Unavailable)
            }
            fn mx(&self, _n: &str, d: Instant) -> Result<Vec<String>, DnsError> {
                std::thread::sleep(d.saturating_duration_since(Instant::now()));
                Err(DnsError::Unavailable)
            }
        }
        let mut rq = req(D);
        rq.timeout_ms = 0; // clamped up to 1 s
        let http = FakeHttp::default().with(ISPDB, GMAIL);
        let t = Instant::now();
        let r = lookup(&rq, &SlowDns, &http);
        assert!(
            t.elapsed() < Duration::from_secs(3),
            "must honour the 1 s floor, not 5 s"
        );
        assert_eq!(r.status, AutoconfigStatus::Offline);
        assert!(http.urls().is_empty(), "no HTTP after the deadline passed");
    }

    #[test]
    fn timeout_is_clamped_to_the_documented_range() {
        assert_eq!(0u64.clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS), 1_000);
        assert_eq!(999_999u64.clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS), 15_000);
    }

    #[test]
    fn cancel_skips_every_source() {
        let cancel = AtomicBool::new(true);
        let http = FakeHttp::default().with(ISPDB, GMAIL);
        let dns = FakeDns::default();
        let r = lookup_cancellable(&req(D), &dns, &http, &cancel);
        assert!(r.tried.iter().all(|a| a.outcome == AttemptOutcome::Skipped));
        assert!(http.urls().is_empty() && dns.log.lock().unwrap().is_empty());
        assert_eq!(r.status, AutoconfigStatus::NotFound);
    }

    #[test]
    fn domain_validation_rejects_addresses_and_junk() {
        for bad in [
            "me@example.org",
            "@example.org",
            "example",
            "example.",
            "a b.org",
            "exa_mple.org",
            "x.test",
            "x.example",
            "x.invalid",
            "x.local",
            "x.localhost",
            "127.0.0.1",
            "example.o",
            "example.123",
            "localhost",
            "-x.org",
            "",
            "ex ample.org",
            "https://example.org",
            "example.org/path",
            "münchen.de",
        ] {
            assert_eq!(normalize_domain(bad), None, "{bad:?}");
        }
        assert_eq!(
            normalize_domain("Example.ORG.").as_deref(),
            Some("example.org")
        );
        assert_eq!(
            normalize_domain("xn--mnchen-3ya.de").as_deref(),
            Some("xn--mnchen-3ya.de")
        );
    }

    #[test]
    fn an_invalid_domain_does_no_io() {
        let http = FakeHttp::default();
        let dns = FakeDns::default();
        let r = lookup(&req("me@example.org"), &dns, &http);
        assert_eq!(r.status, AutoconfigStatus::NotFound);
        assert!(r.tried.is_empty() && http.urls().is_empty() && dns.log.lock().unwrap().is_empty());
    }

    #[test]
    fn request_json_refuses_a_local_part_or_any_unknown_field() {
        for bad in [
            r#"{"domain":"me@example.org"}"#,
            r#"{"domain":"example.org","email":"me@example.org"}"#,
            r#"{"domain":"example.org","local_part":"me"}"#,
            r#"{"domain":"example.org","password":"x"}"#,
            r#"{"domain":"example.org","sources":["pop3"]}"#,
            r#"{"nodomain":1}"#,
            "not json",
            "",
        ] {
            assert!(
                matches!(lookup_json(bad), Err(CoreError::InvalidArgument)),
                "{bad}"
            );
        }
    }

    #[test]
    fn lookup_json_with_every_source_disabled_is_not_found_without_io() {
        let out = lookup_json(r#"{"domain":"example.org","sources":[]}"#).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "not_found");
        assert_eq!(v["tried"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn report_json_shape_matches_the_header_contract() {
        let http = FakeHttp::default().with(ISPDB, FASTMAIL);
        let r = lookup(&req(D), &FakeDns::default(), &http);
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["status"], "found");
        assert_eq!(v["source"], "ispdb");
        assert_eq!(v["imap"]["tls"], "implicit");
        assert_eq!(v["smtp"]["tls"], "starttls");
        assert_eq!(v["username"], "email");
        assert_eq!(v["tried"][0]["source"], "srv");
        assert_eq!(v["tried"][0]["outcome"], "miss");
        assert!(v["preset_id"].is_null() && v["unsupported"].is_null());
    }

    // ---- production HTTP transport against a loopback fixture server (no internet)

    fn serve_once(response: Vec<u8>) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
            let mut req = Vec::new();
            let mut buf = [0u8; 1024];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
            }
            let _ = s.write_all(&response);
            String::from_utf8_lossy(&req).into_owned()
        });
        (format!("http://{addr}/mail/config-v1.1.xml"), h)
    }

    fn ok_response(body: &[u8]) -> Vec<u8> {
        let mut r = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        r.extend_from_slice(body);
        r
    }

    #[test]
    fn ureq_transport_returns_the_body_and_sends_no_identity() {
        let (url, h) = serve_once(ok_response(FASTMAIL.as_bytes()));
        let body = UreqHttp::for_loopback_tests()
            .get(&url, Instant::now() + Duration::from_secs(5))
            .unwrap();
        assert_eq!(body, FASTMAIL.as_bytes());
        let request = h.join().unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /mail/config-v1.1.xml "));
        assert!(!request.contains("cookie") && !request.contains("authorization"));
    }

    #[test]
    fn ureq_transport_caps_the_body_and_maps_404() {
        let (url, _h) = serve_once(ok_response(&vec![b'x'; 100 * 1024]));
        let e = UreqHttp::for_loopback_tests()
            .get(&url, Instant::now() + Duration::from_secs(5))
            .unwrap_err();
        assert_eq!(e, FetchError::Invalid);
        let (url, _h) = serve_once(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        );
        let e = UreqHttp::for_loopback_tests()
            .get(&url, Instant::now() + Duration::from_secs(5))
            .unwrap_err();
        assert_eq!(e, FetchError::Status(404));
    }

    #[test]
    fn ureq_transport_times_out_on_a_silent_server() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/x", listener.local_addr().unwrap());
        let t = Instant::now();
        let e = UreqHttp::for_loopback_tests()
            .get(&url, Instant::now() + Duration::from_millis(400))
            .unwrap_err();
        assert_eq!(e, FetchError::Timeout);
        assert!(t.elapsed() < Duration::from_secs(3));
        drop(listener);
    }

    #[test]
    fn production_transport_refuses_loopback_and_plain_http() {
        // The guarded production transport must not let a hostile domain aim
        // the GET at this machine, and must not accept plain http at all.
        let (url, _h) = serve_once(ok_response(FASTMAIL.as_bytes()));
        let e = UreqHttp::production()
            .get(&url, Instant::now() + Duration::from_secs(2))
            .unwrap_err();
        assert_ne!(e, FetchError::Status(200));
        assert!(matches!(e, FetchError::Invalid | FetchError::Unreachable));
    }
}
