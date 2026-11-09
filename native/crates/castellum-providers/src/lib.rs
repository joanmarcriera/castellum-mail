// Castellum - mail provider abstraction.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// A narrow, synchronous read-only mail provider interface. The sync engine talks
// to `dyn MailProvider`, so IMAP (and later Gmail API / Graph / JMAP) plug in
// behind the same shape, and tests can drive a mock.

use std::fmt;
use std::ops::RangeInclusive;

mod panic_guard;
pub use panic_guard::PanicGuardedProvider;

/// Connection parameters for an IMAP account.
///
/// `Debug` is implemented manually to redact `password` — configs get formatted
/// into logs/errors with `{:?}`, and a plaintext credential must never leak.
#[derive(Clone)]
pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    /// How the connection is secured (implicit TLS, STARTTLS or none).
    /// TLS validation is ON unless `allow_invalid_certs`.
    pub security: TransportSecurity,
    /// Accept invalid/self-signed certificates. MUST default to false; only true
    /// for explicitly user-approved local test profiles (e.g. GreenMail).
    pub allow_invalid_certs: bool,
}

impl fmt::Debug for ImapConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImapConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("security", &self.security)
            .field("allow_invalid_certs", &self.allow_invalid_certs)
            .finish()
    }
}

/// How an IMAP connection is secured (, spec
/// project-docs/specs/2026-10-12/imap-starttls.md). An explicit mode, not a
/// port heuristic: guessing STARTTLS from a non-993 port would silently
/// switch accounts that use implicit TLS on a custom port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportSecurity {
    /// TLS from the first byte (IMAPS, usually port 993).
    ImplicitTls,
    /// Plain connect, then a mandatory STARTTLS upgrade before LOGIN
    /// (usually port 143). Never falls back to cleartext.
    StartTls,
    /// No encryption at all.
    Plaintext,
}

impl TransportSecurity {
    /// The stored-account resolution: `imap_security` ("tls" | "starttls" |
    /// "none") wins; absent, the legacy `use_tls` flag decides. An unknown
    /// value resolves to implicit TLS, so the worst case is a TLS error,
    /// never a cleartext login.
    pub fn resolve(imap_security: Option<&str>, use_tls: bool) -> Self {
        match imap_security.map(|s| s.trim().to_ascii_lowercase()) {
            Some(s) if s == "starttls" => TransportSecurity::StartTls,
            Some(s) if s == "none" => TransportSecurity::Plaintext,
            Some(_) => TransportSecurity::ImplicitTls,
            None if use_tls => TransportSecurity::ImplicitTls,
            None => TransportSecurity::Plaintext,
        }
    }

    /// True for both TLS modes.
    pub fn is_encrypted(self) -> bool {
        !matches!(self, TransportSecurity::Plaintext)
    }
}

/// Connection parameters for an SMTP submission endpoint. Mirrors [`ImapConfig`],
/// including the manual `Debug` that redacts `password`.
#[derive(Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    /// Use TLS (implicit on 465, STARTTLS otherwise). `false` = plaintext —
    /// only for local test servers (e.g. GreenMail on :3025).
    pub use_tls: bool,
    /// Accept invalid/self-signed certificates. MUST default to false; only true
    /// for explicitly user-approved local test profiles.
    pub allow_invalid_certs: bool,
}

impl fmt::Debug for SmtpConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmtpConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("use_tls", &self.use_tls)
            .field("allow_invalid_certs", &self.allow_invalid_certs)
            .finish()
    }
}

/// An outbound message submitter (SMTP or a mock). Deliberately minimal: the
/// caller supplies the envelope (`from` + `rcpts`, so Bcc recipients are
/// delivered without appearing in headers) and the already-built RFC822 bytes.
pub trait SmtpSender {
    /// Submit `raw_message` for delivery to every address in `rcpts`, with
    /// `from` as the envelope sender. `rcpts` must be non-empty.
    fn send(
        &mut self,
        from: &str,
        rcpts: &[String],
        raw_message: &[u8],
    ) -> Result<(), ProviderError>;
}

/// A remote mailbox/folder as reported by the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteMailbox {
    pub path: String,
    pub name: String,
    /// Semantic role if detectable ("inbox", "sent", "drafts", "trash", ...).
    pub role: Option<String>,
    /// How `role` was determined. Provider-declared SPECIAL-USE metadata is
    /// authoritative and must outrank a coincidental well-known folder name
    /// when duplicate role candidates are canonicalised.
    pub role_source: Option<MailboxRoleSource>,
    /// Provider hierarchy delimiter for this mailbox, when reported by LIST/LSUB.
    pub delimiter: Option<String>,
    /// Whether the mailbox appears in LSUB. Providers that cannot distinguish
    /// subscriptions should leave this false and let the sync policy fall back to
    /// Inbox/special-use folders.
    pub subscribed: bool,
    /// Whether the folder can be SELECTed and hold messages. `false` for
    /// container-only nodes (IMAP `\Noselect`, e.g. a parent in a nested tree);
    /// the sync engine records them for hierarchy but never tries to fetch them.
    pub selectable: bool,
}

/// Provenance for a remotely discovered mailbox role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailboxRoleSource {
    /// RFC 6154 SPECIAL-USE (or equivalent provider-declared metadata).
    SpecialUse,
    /// Best-effort inference from the provider-native mailbox name/path.
    Heuristic,
}

/// A system IMAP flag this crate's write path can set on APPEND/STORE.
/// Provider-agnostic: `castellum-provider-imap`'s `SessionFlag` maps 1:1 onto
/// this, and no other provider crate exists yet, but the abstraction stays
/// here (not `imap_types::Flag`) so this trait never depends on a wire crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageFlag {
    Seen,
    Draft,
    Flagged,
    Answered,
    Deleted,
}

/// A message's UID after a successful provider write, when the server
/// reported one (e.g. RFC 4315 UIDPLUS APPENDUID).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppendedUid {
    pub uid_validity: i64,
    pub uid: i64,
}

/// A fetched raw message with its provider UID and flag state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteMessage {
    pub uid: i64,
    pub raw: Vec<u8>,
    pub is_seen: bool,
    /// Remote \Flagged (starred). Wired through sync so stars round-trip.
    pub is_flagged: bool,
}

/// A descriptor for a remote message, fetched without the message body. This is
/// the staged-sync shape: enough envelope metadata for list/thread rows, while
/// body and attachment bytes remain lazy follow-up work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteMessageDescriptor {
    pub uid: i64,
    pub size_bytes: Option<i64>,
    /// Provider-known attachment presence from descriptor-level metadata such as
    /// IMAP BODYSTRUCTURE. `None` means the provider did not report enough
    /// metadata; body parsing may still discover attachments later.
    pub has_attachments: Option<bool>,
    pub is_seen: bool,
    pub is_flagged: bool,
    pub rfc822_message_id: Option<String>,
    pub subject: Option<String>,
    pub from: Option<RemoteAddress>,
    pub to: Vec<RemoteAddress>,
    pub cc: Vec<RemoteAddress>,
    pub bcc: Vec<RemoteAddress>,
    /// Unix epoch seconds from provider metadata, usually IMAP INTERNALDATE.
    pub sent_at: Option<i64>,
    /// Message ids known from descriptor-level metadata. IMAP ENVELOPE provides
    /// In-Reply-To; a future header-field descriptor can add full References.
    pub references: Vec<String>,
    /// IMAP keywords the provider reported for this message, or `None` when
    /// the provider did not report keywords at all (leave local keyword
    /// state alone — never wipe it). `Some(vec![])` means the provider
    /// positively reported zero keywords.
    pub keywords: Option<Vec<String>>,
}

/// Why a chunked descriptor fetch skipped one UID instead of returning its
/// descriptor. A fixed, closed set of classifications only — never the
/// underlying `SessionError`/server text, which must not reach a durable
/// diagnostics table or a sync report (same discipline as
/// `castellum_core::redact`, which this variant set mirrors for exactly one
/// case: a response byte-stream `imap-codec` could not decode at all).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescriptorSkipReason {
    /// The server's FETCH response for this UID, fetched alone after the
    /// sub-batch holding it had already failed, could not be decoded.
    Undecodable,
}

impl DescriptorSkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DescriptorSkipReason::Undecodable => "message could not be decoded and was skipped",
        }
    }
}

impl fmt::Display for DescriptorSkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One UID a chunked descriptor fetch could not decode and skipped rather
/// than failing its whole chunk (CLAUDE.md gotcha 1's "never fail the folder
/// for one message", applied per-message by isolation — see
/// `MailProvider::fetch_descriptor_chunk`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkippedDescriptor {
    pub uid: i64,
    pub reason: DescriptorSkipReason,
}

/// Why a descriptor fetch (`fetch_descriptor_chunk`, `fetch_descriptor_list`,
/// `retry_descriptor_skips`) stopped before covering every requested UID. A
/// closed set of classifications, never server text (same discipline as
/// [`DescriptorSkipReason`]). Every variant is a per-pass limit, not a
/// verdict on the mailbox: the next sync pass resumes just past
/// [`DescriptorFetchIncomplete::covered_through`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescriptorFetchStop {
    /// Isolating undecodable messages used every reconnect this pass allows
    /// except the one held in reserve; that reserve was spent on a fresh,
    /// re-selected connection, so the session is CLEAN and the rest of the
    /// pass can continue with other mailboxes.
    ReconnectBudgetExhausted,
    /// This one call already skipped as many UIDs as it may (see the
    /// provider's per-call skip cap); the session was left clean.
    SkipBudgetExceeded,
    /// A decode failure poisoned the session and no reconnect was left (or
    /// the reconnect itself failed). `MailProvider::session_abandoned` now
    /// returns true and every later call fails fast: the caller must end the
    /// pass instead of failing each remaining mailbox at SELECT.
    SessionAbandoned,
    /// Isolation had already started when a failure that is NOT a decode
    /// failure hit — the connection dropped, the server sent BYE, a command
    /// was refused, or a re-SELECT after a reconnect was. What was covered
    /// before it is kept (review of PR #180, round 3: returning an `Err`
    /// threw away skips that had already cost reconnects). Check
    /// `session_abandoned()`: after a dropped connection it is true.
    Interrupted,
}

impl DescriptorFetchStop {
    pub fn as_str(self) -> &'static str {
        match self {
            DescriptorFetchStop::ReconnectBudgetExhausted => {
                "descriptor fetch stopped: reconnect budget for undecodable messages exhausted"
            }
            DescriptorFetchStop::SkipBudgetExceeded => {
                "descriptor fetch stopped: too many undecodable messages in one request"
            }
            DescriptorFetchStop::SessionAbandoned => {
                "descriptor fetch stopped: session abandoned after an undecodable response"
            }
            DescriptorFetchStop::Interrupted => {
                "descriptor fetch stopped: interrupted by a network or server error"
            }
        }
    }
}

impl fmt::Display for DescriptorFetchStop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why [`MailProvider::session_abandoned`] is true, when it is (the tracker
/// #2638 — the account-failure classification used to read every one of
/// these as "protocol"). Only [`Poisoned`] should still read that way; the
/// other two are connection-shaped failures, not the decode error that may
/// have started the recovery attempt.
///
/// [`Poisoned`]: SessionAbandonReason::Poisoned
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAbandonReason {
    /// The connection is gone: a server `BYE` mid-command, a dropped TCP
    /// connection, or an I/O EOF/error. Never a decode failure.
    Closed,
    /// The session sits poisoned by an undecodable response and nothing
    /// ever attempted (or got the chance) to replace it with a fresh one.
    Poisoned,
    /// A recovery attempt explicitly gave up: either the reconnect budget
    /// was already spent, or the reconnect it spent trying to replace a
    /// poisoned/closed session itself failed (TCP/TLS/LOGIN). Either way
    /// the account could not get a working session back — a network-shaped
    /// failure, whatever originally poisoned or closed the old one.
    BudgetExhausted,
}

/// How far a descriptor fetch got before it stopped early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DescriptorFetchIncomplete {
    /// The highest requested UID such that EVERY requested UID at or below
    /// it was either returned in `descriptors`, recorded in `skipped`, or
    /// does not exist on the server. `None` = nothing covered. A caller may
    /// advance a durable checkpoint to this UID and no further — it never
    /// passes a UID nothing durable remembers.
    pub covered_through: Option<i64>,
    pub reason: DescriptorFetchStop,
}

/// The result of one bounded descriptor fetch: every descriptor the provider
/// could decode among the requested UIDs, plus any UID it isolated and
/// skipped instead. Never partially applied silently — a caller that ignores
/// `skipped` still gets every decodable descriptor, and `incomplete` says
/// exactly how much of the request is covered when the provider stopped
/// early (review of PR #180: progress made before a budget stop must be kept,
/// not thrown away with an error).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DescriptorChunkResult {
    pub descriptors: Vec<RemoteMessageDescriptor>,
    pub skipped: Vec<SkippedDescriptor>,
    /// `None`: the whole request is covered. `Some`: the provider stopped
    /// early; see [`DescriptorFetchIncomplete`].
    pub incomplete: Option<DescriptorFetchIncomplete>,
}

/// Address metadata from a provider envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAddress {
    pub name: Option<String>,
    pub address: String,
}

/// A cheap UID + flag descriptor (no body) for reconciliation. Fetched with
/// `UID FETCH 1:* (FLAGS)` — the sync engine diffs these against local rows to
/// detect remote flag changes and vanished (expunged/moved) messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UidFlags {
    pub uid: i64,
    pub is_seen: bool,
    pub is_flagged: bool,
    /// See `RemoteMessageDescriptor::keywords`: `None` = not reported, keep
    /// local keyword state; `Some(vec![])` = the provider has none.
    pub keywords: Option<Vec<String>>,
}

/// Outcome of `MailProvider::store_keyword`: whether the write reached the
/// server or was refused locally because the mailbox's PERMANENTFLAGS does
/// not allow this keyword (plan Assumption 5 — this is never an error, the
/// caller falls back to a local-only tag).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeywordStoreOutcome {
    Stored,
    NotPermanent,
}

/// What woke an `idle_wait` call. Deliberately coarse: the caller always
/// runs a targeted resync on `Activity` rather than trying to interpret
/// EXISTS/EXPUNGE counts itself — `select_qresync`/`fetch_new_descriptors`
/// figure out exactly what changed far more precisely than a raw IDLE
/// notification can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleWake {
    Activity,
    Timeout,
}

/// Result of `MailProvider::select_qresync` — the SELECT itself plus every
/// CONDSTORE/QRESYNC delta the server piggy-backed on its completion (RFC
/// 7162 §3.6.1): flag changes on messages the client already knows about, and
/// UIDs that vanished since the caller's last known `highest_modseq`.
/// Brand-new messages are NOT included here — `fetch_new_descriptors` still
/// finds those from `state.uid_next`, exactly as on the non-QRESYNC path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QresyncSync {
    pub state: MailboxState,
    /// The server's new HIGHESTMODSEQ for this mailbox, to persist as the
    /// next sync's floor. `None` only if the server didn't report one despite
    /// QRESYNC being enabled; callers must treat that as "can't trust
    /// incremental sync next time" and fall back to a full resync.
    pub highest_modseq: Option<i64>,
    pub changed: Vec<UidFlags>,
    /// UIDs expunged since the caller's `highest_modseq` (`VANISHED
    /// (EARLIER)`), kept as the server's ranges — may span far more UIDs than
    /// the mailbox ever held, so intersect it with local rows, never iterate it.
    pub vanished: UidRangeSet,
}

/// A set of UIDs held as sorted, disjoint, non-adjacent inclusive ranges —
/// the shape a QRESYNC `VANISHED` response carries on the wire. Deliberately
/// never expanded into individual UIDs: `1:4294967295` is 13 bytes on the
/// wire and would be ~16 GiB as a `Vec<u32>`, so a hostile or careless server
/// could exhaust memory with one line (CLAUDE.md gotcha 1: the wire must
/// never crash us). Memory is O(ranges received) and membership is
/// O(log ranges); consumers intersect it with the UIDs they hold locally
/// instead of walking it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UidRangeSet {
    /// Invariant: sorted by start, disjoint, non-adjacent, start <= end.
    ranges: Vec<RangeInclusive<i64>>,
}

impl UidRangeSet {
    /// Build from ranges in any order. A reversed range (`5..=3`, IMAP's
    /// `5:3`) means the same as `3..=5`; overlapping or touching ranges are
    /// merged, so the result obeys the type's invariant.
    pub fn from_ranges(ranges: impl IntoIterator<Item = RangeInclusive<i64>>) -> Self {
        let mut spans: Vec<(i64, i64)> = ranges
            .into_iter()
            .map(|r| {
                let (a, b) = r.into_inner();
                (a.min(b), a.max(b))
            })
            .collect();
        spans.sort_unstable();
        let mut merged: Vec<RangeInclusive<i64>> = Vec::with_capacity(spans.len());
        for (start, end) in spans {
            match merged.last_mut() {
                // Overlapping or adjacent: extend the previous range.
                Some(last) if start <= last.end().saturating_add(1) => {
                    if end > *last.end() {
                        *last = *last.start()..=end;
                    }
                }
                _ => merged.push(start..=end),
            }
        }
        UidRangeSet { ranges: merged }
    }

    /// The normalised ranges (sorted, disjoint, non-adjacent).
    pub fn ranges(&self) -> &[RangeInclusive<i64>] {
        &self.ranges
    }

    pub fn contains(&self, uid: i64) -> bool {
        // The first range ending at or after `uid` is the only candidate.
        let i = self.ranges.partition_point(|r| *r.end() < uid);
        self.ranges.get(i).is_some_and(|r| *r.start() <= uid)
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }
}

/// Collect individual UIDs (each a one-UID range) into a normalised set.
impl FromIterator<i64> for UidRangeSet {
    fn from_iter<I: IntoIterator<Item = i64>>(iter: I) -> Self {
        Self::from_ranges(iter.into_iter().map(|uid| uid..=uid))
    }
}

/// Provider-reported state of a selected mailbox (IMAP UIDVALIDITY/UIDNEXT).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailboxState {
    /// UIDVALIDITY, or a negative value if the server did not report one
    /// (treated as "unknown" — forces a full refetch).
    pub uid_validity: i64,
    pub uid_next: i64,
    /// Number of messages currently in the mailbox (IMAP EXISTS).
    pub exists: i64,
}

/// Outcome of a UIDVALIDITY-guarded write (`MailProvider::*_checked`). A UID
/// names a message only inside one UIDVALIDITY; when the mailbox's current
/// value is not the one the caller recorded, the UID may name a different
/// message, so the write is refused before anything is sent for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guarded<T> {
    /// UIDVALIDITY matched and the write ran.
    Applied(T),
    /// Refused: the mailbox reported `current` (non-positive = the server
    /// reported none), which is not the expected value. Nothing was written.
    UidValidityChanged { current: i64 },
}

/// Where a moved message landed, from the server's COPYUID. Recorded on the
/// `move` op so Undo can address the message by this exact UID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MovedTo {
    pub uid_validity: i64,
    pub uid: i64,
}

/// Whether two Message-ID header values name the same message, EXACTLY:
/// whitespace and angle brackets are ignored and the domain (after the last
/// `@`) is compared case-insensitively, the local part case-sensitively
/// (RFC 5322 leaves the local part opaque). Empty never matches. A server's
/// `SEARCH HEADER` is a substring match, so every candidate it returns must
/// pass this before anything is moved on its strength.
pub fn message_ids_match(a: &str, b: &str) -> bool {
    fn norm(s: &str) -> Option<(String, String)> {
        let t = s
            .trim()
            .trim_start_matches('<')
            .trim_end_matches('>')
            .trim();
        if t.is_empty() {
            return None;
        }
        Some(match t.rfind('@') {
            Some(i) => (t[..i].to_string(), t[i + 1..].to_lowercase()),
            None => (t.to_string(), String::new()),
        })
    }
    matches!((norm(a), norm(b)), (Some(x), Some(y)) if x == y)
}

/// Whether a write addressed by UID may proceed: both values must be known
/// (positive) and equal. `0` is the DB's "no sync cursor yet" sentinel and
/// `-1` is "the server reported no UIDVALIDITY"; neither ever matches, not
/// even itself.
pub fn uid_validity_matches(expected: i64, current: i64) -> bool {
    expected > 0 && current > 0 && expected == current
}

/// Every UID carrying one RFC822 Message-ID in a mailbox, with the
/// UIDVALIDITY of the SELECT that found them
/// (`MailProvider::find_uids_by_message_id`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageIdMatches {
    /// Non-positive when the server reported none.
    pub uid_validity: i64,
    /// Ascending; empty when nothing matched.
    pub uids: Vec<i64>,
}

/// Typed provider errors. Kept coarse for slice 4; expand as providers grow.
#[derive(Debug)]
pub enum ProviderError {
    Auth(String),
    /// The server refused SMTP AUTH because authenticated SMTP submission is
    /// switched off for the mailbox or the organisation (Microsoft: `535
    /// 5.7.139 ... SmtpClientAuthentication is disabled`). The credential is
    /// not at fault, so refreshing a token or retrying cannot help; only a
    /// change of that setting can. See project-docs/microsoft-send-2026-10.md.
    AuthSmtpDisabled(String),
    Tls(String),
    Network(String),
    Protocol(String),
    Unsupported(String),
    /// The SMTP server refused the message itself with a permanent (5xx)
    /// reply that is not an authentication failure: an unknown recipient,
    /// a policy or spam rejection, a message too large. Sending the same
    /// bytes again gets the same answer, so no automatic retry (the tracker
    /// 3632, G8). The detail is the server's own text.
    Rejected(String),
    /// The SMTP server answered with a transient (4xx) reply: greylisting,
    /// a full mailbox or spool, rate limiting. The server was reached and is
    /// talking, so this is not a network fault, but the reply says "try
    /// again later": it is retried on the normal backoff. The detail is the
    /// server's own text. Never produced by IMAP.
    Deferred(String),
    /// An SMTP submission failed in a way that leaves its outcome unknown:
    /// the session was up and the failure is not a server reply (the
    /// connection dropped or timed out, or the reply never arrived whole),
    /// so the server may already hold the message it received after DATA.
    /// Retrying could deliver a second copy; the send executor holds it for
    /// the user instead ("may have been sent"). Never produced by IMAP.
    MaybeDelivered(String),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::Auth(m) => write!(f, "auth error: {m}"),
            ProviderError::AuthSmtpDisabled(m) => write!(f, "SMTP AUTH disabled: {m}"),
            ProviderError::Tls(m) => write!(f, "tls error: {m}"),
            ProviderError::Network(m) => write!(f, "network error: {m}"),
            ProviderError::Protocol(m) => write!(f, "protocol error: {m}"),
            ProviderError::Unsupported(m) => write!(f, "unsupported: {m}"),
            ProviderError::Rejected(m) => write!(f, "rejected: {m}"),
            ProviderError::Deferred(m) => write!(f, "deferred: {m}"),
            ProviderError::MaybeDelivered(m) => write!(f, "may have been delivered: {m}"),
        }
    }
}

impl std::error::Error for ProviderError {}

/// A mail provider. Sync (`fetch_new`) is strictly read-only (`BODY.PEEK`, no
/// flag changes). The only remote mutation is `store_seen` (write-path replay),
/// which is idempotent; everything else must not alter remote state.
pub trait MailProvider {
    /// List all mailboxes/folders.
    fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError>;

    /// Select a mailbox for subsequent fetches; returns its UID state.
    fn select(&mut self, path: &str) -> Result<MailboxState, ProviderError>;

    /// Fetch messages with UID strictly greater than `after_uid`
    /// (`None` = fetch all). Must not alter message flags.
    fn fetch_new(&mut self, after_uid: Option<i64>) -> Result<Vec<RemoteMessage>, ProviderError>;

    /// Fetch message descriptors with UID strictly greater than `after_uid`,
    /// without full message bodies. Providers should use this before `fetch_new`
    /// when possible so mature mailboxes can become visible without downloading
    /// every body and attachment.
    fn fetch_new_descriptors(
        &mut self,
        _after_uid: Option<i64>,
    ) -> Result<Vec<RemoteMessageDescriptor>, ProviderError> {
        Err(ProviderError::Unsupported(
            "fetch_new_descriptors not supported by this provider".into(),
        ))
    }

    /// Fetch descriptors for UIDs in the inclusive range `low..=high` only —
    /// the bounded counterpart of `fetch_new_descriptors`.
    /// The sync engine calls this in ~1000-UID chunks across a mailbox's
    /// first/from-scratch import instead of one unbounded `low:*` fetch, so a
    /// large mailbox checkpoints and reports progress every chunk instead of
    /// buffering the whole response before any row is written.
    ///
    /// Unlike `fetch_new_descriptors`, a single undecodable message in the
    /// range must NEVER fail the call or poison the mailbox's import: an
    /// implementation that cannot decode part of the range should isolate
    /// the bad UID (reconnecting, since a decode failure poisons the
    /// underlying session — CLAUDE.md gotcha 1) and report it in the
    /// returned `skipped` list instead. An implementation that must bound
    /// that work (reconnects cost a LOGIN each) returns what it covered so
    /// far with `incomplete` set — never an `Err` that throws the progress
    /// away. `mailbox_path` is passed on every call (not just cached from an
    /// earlier `select`) so an implementation that must reconnect mid-call
    /// can re-select it.
    ///
    /// Defaults to `Unsupported`; the sync engine falls back to
    /// `fetch_new_descriptors` (unbounded) for a provider that returns it.
    fn fetch_descriptor_chunk(
        &mut self,
        _mailbox_path: &str,
        _low: i64,
        _high: i64,
    ) -> Result<DescriptorChunkResult, ProviderError> {
        Err(ProviderError::Unsupported(
            "fetch_descriptor_chunk not supported by this provider".into(),
        ))
    }

    /// Every UID the server currently holds in `low..=high` in the selected
    /// mailbox, ascending (`UID SEARCH UID low:high`, or any other encoding
    /// the provider prefers)., Opus review: the sync engine
    /// calls this once instead of walking a large UID span in
    /// `DESCRIPTOR_CHUNK_SIZE`-sized windows, so a sparse mailbox (a
    /// heavily-expunged Spam folder with 300 messages spread across a
    /// UIDNEXT in the millions) chunks over the messages that actually
    /// exist rather than over mostly-empty UID windows.
    ///
    /// The result MUST lie within `low..=high`: RFC 3501 §6.4.8 makes
    /// `UID SEARCH UID n:*` name the mailbox's LAST UID even when it is
    /// below `n`, and an implementation must filter that (or any other
    /// out-of-span UID a server returns) rather than hand it back — the
    /// sync engine would otherwise re-import an old message as new (review
    /// of PR #180, nit 4). Read-only. Defaults to `Unsupported`; the sync
    /// engine then walks the UID range directly via `fetch_descriptor_chunk`.
    fn search_uids_in(&mut self, _low: i64, _high: i64) -> Result<Vec<i64>, ProviderError> {
        Err(ProviderError::Unsupported(
            "search_uids_in not supported by this provider".into(),
        ))
    }

    /// Fetch descriptors for exactly the given (not necessarily contiguous)
    /// UIDs — the sparse counterpart of `fetch_descriptor_chunk`, used after
    /// `search_uids_in` to fetch one ~`DESCRIPTOR_CHUNK_SIZE`-sized group of
    /// the UIDs it returned. Same isolation/skip/`incomplete` contract as
    /// `fetch_descriptor_chunk`: a single undecodable message must never
    /// fail the whole call, only be isolated and reported in `skipped`.
    /// Defaults to `Unsupported`.
    fn fetch_descriptor_list(
        &mut self,
        _mailbox_path: &str,
        _uids: &[i64],
    ) -> Result<DescriptorChunkResult, ProviderError> {
        Err(ProviderError::Unsupported(
            "fetch_descriptor_list not supported by this provider".into(),
        ))
    }

    /// Re-fetch UIDs a previous pass skipped as undecodable (the sync
    /// engine's decode-skip retry set), one at a time, on a reconnect budget
    /// of its OWN — separate from, and never spending, the budget that
    /// isolates bad messages during import (review of PR #180, finding 3:
    /// retries sharing that budget could starve or poison the import).
    ///
    /// In the result, `descriptors` are UIDs that now decode, `skipped` are
    /// UIDs attempted that still do not, and any other attempted UID no
    /// longer exists on the server. `incomplete.covered_through` (when set)
    /// is the highest UID actually attempted; UIDs above it were not tried
    /// this pass and must be left as they are. Defaults to `Unsupported`,
    /// which the sync engine treats as "no retries this pass".
    fn retry_descriptor_skips(
        &mut self,
        _mailbox_path: &str,
        _uids: &[i64],
    ) -> Result<DescriptorChunkResult, ProviderError> {
        Err(ProviderError::Unsupported(
            "retry_descriptor_skips not supported by this provider".into(),
        ))
    }

    /// Lowest UID in the currently selected mailbox whose INTERNALDATE is on
    /// or after `imap_date` (an RFC 3501 `date` such as `23-Jun-2026`), via
    /// `UID SEARCH SINCE`. `Ok(None)` = no message that recent. Used by the
    /// first sync of a mailbox to honour an "only the last N months" initial
    /// history window (S62) without downloading the older descriptors.
    /// Read-only. Defaults to `Unsupported`, which the sync engine treats as
    /// "no window available — sync everything", matching pre-S62 behaviour.
    fn first_uid_since(&mut self, _imap_date: &str) -> Result<Option<i64>, ProviderError> {
        Err(ProviderError::Unsupported(
            "first_uid_since not supported by this provider".into(),
        ))
    }

    /// Fetch full raw messages for a bounded set of provider UIDs in the
    /// currently selected mailbox. Used after descriptor import to hydrate
    /// missing bodies without scanning or downloading the whole mailbox.
    fn fetch_bodies(&mut self, _uids: &[i64]) -> Result<Vec<RemoteMessage>, ProviderError> {
        Err(ProviderError::Unsupported(
            "fetch_bodies not supported by this provider".into(),
        ))
    }

    /// Fetch one MIME body part by its IMAP BODYSTRUCTURE section path (e.g.
    /// "2", "1.2") for a single message by UID, without downloading the rest
    /// of the message. Read-only (`BODY.PEEK`, never sets `\Seen`). `section`
    /// is never empty — callers resolve it from `AttachmentDescriptor::section`
    /// before calling; an empty `section` is a caller bug, not a legal
    /// "whole body" request (use `fetch_bodies` for that). Returns the part
    /// body exactly as it is on the wire, i.e. STILL in its
    /// `Content-Transfer-Encoding` (base64 / quoted-printable): decoding is the
    /// caller's job, since only it knows the encoding from the message's
    /// hydrated `AttachmentDescriptor`. Defaults to an error for providers
    /// that don't support partial fetch.
    fn fetch_attachment_part(
        &mut self,
        _uid: i64,
        _section: &str,
    ) -> Result<Vec<u8>, ProviderError> {
        Err(ProviderError::Unsupported(
            "fetch_attachment_part not supported by this provider".into(),
        ))
    }

    /// Snapshot the UID + flag descriptors of every message in the selected
    /// mailbox (`UID FETCH 1:* (FLAGS)`, no bodies). Used for RFC 4549-style
    /// reconciliation: the sync engine diffs this against local rows to apply
    /// remote flag changes and remove vanished messages.
    ///
    /// Defaults to an error. The sync engine treats ANY error as "reconciliation
    /// unsupported/unavailable this run" and skips it — it must NEVER interpret a
    /// failure as an empty mailbox (that would purge every local row). Read-only.
    fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
        Err(ProviderError::Unsupported(
            "list_uid_flags not supported by this provider".into(),
        ))
    }

    /// Whether this connected session negotiated CONDSTORE/QRESYNC (RFC 7162)
    /// — i.e. `ENABLE CONDSTORE QRESYNC` succeeded at connect time. Read-only,
    /// no command issued. Defaults to `false` for providers that never
    /// negotiate it (mocks, a future non-IMAP provider).
    fn qresync_available(&self) -> bool {
        false
    }

    /// The HIGHESTMODSEQ the most recent `select` call on THIS provider
    /// instance captured from the server's untagged `[HIGHESTMODSEQ n]` code,
    /// if CONDSTORE/QRESYNC is negotiated and the server reported one. A
    /// side-channel accessor over `select`'s own response — issues no command.
    /// Always call `select` (or `select_qresync`) first. `None` when
    /// `qresync_available()` is false, before any `select` call, or if the
    /// server sent `NOMODSEQ` for this mailbox.
    fn last_highest_modseq(&self) -> Option<i64> {
        None
    }

    /// SELECT a mailbox with QRESYNC parameters (RFC 7162 §3.6.2), resuming
    /// from `uid_validity`/`highest_modseq`. If the server's current
    /// UIDVALIDITY doesn't match, the server silently performs an ordinary
    /// SELECT (no VANISHED/changed-FETCH data) rather than erroring —
    /// `Ok(QresyncSync)` still comes back, but with an empty `changed`/
    /// `vanished` and a `state.uid_validity` that no longer matches the
    /// caller's; callers must compare `state.uid_validity` before trusting
    /// the delta as complete. Defaults to `Unsupported` so callers always
    /// have a full-resync fallback available.
    fn select_qresync(
        &mut self,
        _mailbox_path: &str,
        _uid_validity: i64,
        _highest_modseq: i64,
    ) -> Result<QresyncSync, ProviderError> {
        Err(ProviderError::Unsupported(
            "select_qresync not supported by this provider".into(),
        ))
    }

    /// True once this session is unusable and every later call will fail
    /// fast: [`PanicGuardedProvider`] after a panic in the protocol parser
    /// desynchronised the connection, and an IMAP provider whose session an
    /// undecodable response poisoned with no reconnect left to replace it
    /// (review of PR #180, finding 2 — a poisoned session left in place with
    /// this still `false` failed every later mailbox at SELECT).
    ///
    /// Callers that treat a per-item failure as recoverable (sync skips a
    /// mailbox and continues) MUST consult this: once the session is
    /// abandoned, continuing is pointless, and treating the resulting run as a
    /// success would clear the account's error state after a crash. Ordinary
    /// providers never abandon, so the default is `false`.
    fn session_abandoned(&self) -> bool {
        false
    }

    /// Why `session_abandoned()` is true, when it is. The
    /// sync engine maps this to the account failure's `error_kind`: a closed
    /// connection or a recovery attempt that itself could not get a working
    /// session back reads as "network" (the account can just retry); only a
    /// session left poisoned by an undecodable response, with nothing
    /// having tried to replace it, still reads as "protocol" — see
    /// [`SessionAbandonReason`]. `None` when the session is not abandoned.
    ///
    /// Default: any abandonment reads as [`SessionAbandonReason::Poisoned`],
    /// matching this crate's and every provider's behaviour before this
    /// method existed. A provider distinguishing its causes (like
    /// `ImapProvider`) overrides this; one that only ever abandons for a
    /// decode failure (or never abandons) does not need to.
    fn session_abandon_reason(&self) -> Option<SessionAbandonReason> {
        self.session_abandoned()
            .then_some(SessionAbandonReason::Poisoned)
    }

    /// The server's CAPABILITY tokens as captured at connection time, for
    /// diagnostics. Implementations should cache them during connect rather
    /// than issue a mid-session command; `None` = not captured/unsupported.
    /// Tokens are IANA-registered protocol atoms (no user data), but callers
    /// persisting them must still sanitise (charset/length/count caps).
    fn capabilities(&self) -> Option<Vec<String>> {
        None
    }

    /// Cleanly close the session.
    fn logout(&mut self) -> Result<(), ProviderError>;

    /// Write path: set (`seen = true`) or clear the `\Seen` flag on a message by
    /// UID in `mailbox_path`. Idempotent, so replaying a queued op is safe.
    /// Defaults to an error for read-only providers.
    fn store_seen(
        &mut self,
        _mailbox_path: &str,
        _uid: i64,
        _seen: bool,
    ) -> Result<(), ProviderError> {
        Err(ProviderError::Unsupported(
            "store_seen not supported by this provider".into(),
        ))
    }

    /// Write path: set `\Seen` on every message in `mailbox_path` (a single
    /// `UID STORE 1:* +FLAGS (\Seen)`). Idempotent — re-running is harmless.
    /// Defaults to an error for read-only providers.
    fn mark_all_seen(&mut self, _mailbox_path: &str) -> Result<(), ProviderError> {
        Err(ProviderError::Unsupported(
            "mark_all_seen not supported by this provider".into(),
        ))
    }

    /// Write path: set `\Seen` on EXACTLY the messages in `uids` (a
    /// non-empty IMAP sequence-set string such as `"3,5:7"`) in
    /// `mailbox_path`, as one `UID STORE <set> +FLAGS (\Seen)`. Used by a
    /// queued Mark All as Read, whose caller snapshotted `uids` when the user
    /// pressed it and has already checked that the mailbox's UIDVALIDITY
    /// still matches, so mail that arrived since is never marked read.
    /// Idempotent. Defaults to an error for read-only providers.
    fn mark_seen_uids(&mut self, _mailbox_path: &str, _uids: &str) -> Result<(), ProviderError> {
        Err(ProviderError::Unsupported(
            "mark_seen_uids not supported by this provider".into(),
        ))
    }

    /// Write path: set/clear `\Flagged` on a message by UID. Idempotent.
    fn store_flagged(
        &mut self,
        _mailbox_path: &str,
        _uid: i64,
        _flagged: bool,
    ) -> Result<(), ProviderError> {
        Err(ProviderError::Unsupported(
            "store_flagged not supported by this provider".into(),
        ))
    }

    /// Write path: set/clear an IMAP keyword on a message by UID. Returns
    /// `NotPermanent` (never an error) when the mailbox's PERMANENTFLAGS
    /// refuses this keyword — see plan Assumption 5. Idempotent.
    fn store_keyword(
        &mut self,
        _mailbox_path: &str,
        _uid: i64,
        _keyword: &str,
        _add: bool,
    ) -> Result<KeywordStoreOutcome, ProviderError> {
        Err(ProviderError::Unsupported(
            "store_keyword not supported by this provider".into(),
        ))
    }

    /// Write path: move a message (identified by UID in `src_path`, and by its
    /// RFC822 `message_id` for idempotency) to `dest_path`. Used for archive and
    /// delete-to-Trash. MUST be idempotent: if a prior attempt already placed the
    /// message in the destination, re-running only ensures the source copy is
    /// gone — it never duplicates. `message_id` may be empty (best-effort guard).
    fn move_message(
        &mut self,
        _src_path: &str,
        _uid: i64,
        _message_id: &str,
        _dest_path: &str,
    ) -> Result<(), ProviderError> {
        Err(ProviderError::Unsupported(
            "move_message not supported by this provider".into(),
        ))
    }

    /// UIDs in `mailbox_path` whose Message-ID header EXACTLY equals
    /// `message_id` (see [`message_ids_match`]; ascending). Read-only.
    /// Implementations must verify each candidate's real header, never trust
    /// a substring SEARCH. Used by `undo_move` to find a message that was
    /// moved earlier when its destination UID was not recorded. May return
    /// several UIDs (duplicates) or none.
    fn locate_by_message_id(
        &mut self,
        _mailbox_path: &str,
        _message_id: &str,
    ) -> Result<Vec<i64>, ProviderError> {
        Err(ProviderError::Protocol("locate unsupported".into()))
    }

    /// Write path: permanently remove EXACTLY the messages in `uids` from
    /// `mailbox_path` (`UID STORE +FLAGS.SILENT (\Deleted)` then expunge).
    /// Destructive — used only for Empty Trash. `uids` is a non-empty IMAP
    /// sequence-set string (e.g. `"3,5:7,10"`) snapshotted by the caller
    /// BEFORE this call, from exactly what the user saw as Trash contents;
    /// the caller has already verified the mailbox's current UIDVALIDITY
    /// still matches that snapshot (see `castellum_core::replay`), so this
    /// method never re-derives "everything in the mailbox" on its own. A
    /// message that arrived after the snapshot is never in `uids` and is
    /// therefore never touched, no matter when replay actually runs.
    fn expunge_mailbox(&mut self, _mailbox_path: &str, _uids: &str) -> Result<(), ProviderError> {
        Err(ProviderError::Unsupported(
            "expunge_mailbox not supported by this provider".into(),
        ))
    }

    /// Write path: APPEND a complete RFC822 message to `mailbox_path`, creating
    /// the mailbox if the server lacks it. The copy is stored `\Seen` — it is
    /// the user's own sent mail, never new mail. `message_id` is the idempotency
    /// guard: when a message with that RFC822 Message-ID is already present in
    /// the mailbox, the append is skipped, so a retried `sent_copy` op never
    /// duplicates the copy. It may be empty (best-effort guard only).
    /// Defaults to an error for read-only providers.
    fn append(
        &mut self,
        _mailbox_path: &str,
        _message_id: &str,
        _raw: &[u8],
    ) -> Result<(), ProviderError> {
        Err(ProviderError::Unsupported(
            "append not supported by this provider".into(),
        ))
    }

    /// Whether this connected session's server advertised the IDLE
    /// capability (RFC 2177). Read-only, no command issued.
    fn idle_supported(&self) -> bool {
        false
    }

    /// SELECT `mailbox_path` and send IDLE, blocking until the server's `+`
    /// continuation confirms idling has begun. Must be paired with exactly
    /// one later `idle_done` before any other command is sent on this
    /// provider.
    fn idle_start(&mut self, _mailbox_path: &str) -> Result<(), ProviderError> {
        Err(ProviderError::Unsupported(
            "idle not supported by this provider".into(),
        ))
    }

    /// Block for up to `timeout` waiting for untagged server activity while
    /// idling. `Ok(IdleWake::Timeout)` on a bare timeout with nothing to
    /// report — never an error. Must only be called between `idle_start` and
    /// `idle_done`.
    fn idle_wait(&mut self, _timeout: std::time::Duration) -> Result<IdleWake, ProviderError> {
        Err(ProviderError::Unsupported(
            "idle not supported by this provider".into(),
        ))
    }

    /// Send DONE and read IDLE's tagged completion, returning the connection
    /// to the ordinary command state.
    fn idle_done(&mut self) -> Result<(), ProviderError> {
        Err(ProviderError::Unsupported(
            "idle not supported by this provider".into(),
        ))
    }

    /// Write path: APPEND `raw` to `mailbox_path` with exactly `flags` set
    /// (unlike `append`, which hardcodes `\Seen`), creating the mailbox if
    /// the server lacks it. Returns the appended message's UID when the
    /// server reports one (RFC 4315 UIDPLUS APPENDUID); `None` means the
    /// caller must resolve it another way (see `find_uid_by_message_id`).
    /// Used for server-draft saves, which need `\Draft` instead of `\Seen`.
    fn append_with_flags_get_uid(
        &mut self,
        _mailbox_path: &str,
        _flags: &[MessageFlag],
        _raw: &[u8],
    ) -> Result<Option<AppendedUid>, ProviderError> {
        Err(ProviderError::Unsupported(
            "append_with_flags_get_uid not supported by this provider".into(),
        ))
    }

    /// Write path: find a message's UID in `mailbox_path` by its RFC822
    /// Message-ID header. The APPENDUID fallback when a server doesn't
    /// report one at APPEND time. `None` = not found (or `message_id` is
    /// empty). Read-only (a SELECT + SEARCH), but grouped with the write
    /// methods because it exists only to support them.
    fn find_uid_by_message_id(
        &mut self,
        _mailbox_path: &str,
        _message_id: &str,
    ) -> Result<Option<AppendedUid>, ProviderError> {
        Err(ProviderError::Unsupported(
            "find_uid_by_message_id not supported by this provider".into(),
        ))
    }

    /// Write path: permanently remove exactly one message by UID from
    /// `mailbox_path` (`STORE \Deleted` + an expunge scoped to that UID when
    /// the server supports UIDPLUS, or an ordinary `EXPUNGE` otherwise).
    /// Idempotent: a UID that no longer exists is a benign no-op, not an
    /// error. Used to remove a superseded or discarded server draft copy.
    fn delete_uid(&mut self, _mailbox_path: &str, _uid: i64) -> Result<(), ProviderError> {
        Err(ProviderError::Unsupported(
            "delete_uid not supported by this provider".into(),
        ))
    }

    // --- UIDVALIDITY-guarded writes -------------------
    //
    // Each `*_checked` method is its unchecked twin, refused with
    // `Guarded::UidValidityChanged` when the mailbox's UIDVALIDITY is not
    // `expected_uid_validity` (see `uid_validity_matches`). The defaults
    // SELECT, compare, then call the unchecked method, which is correct for
    // any provider but leaves a second SELECT between check and write; a
    // real provider overrides them to compare on the SELECT the write itself
    // needs (`ImapProvider` does).

    /// [`Self::store_seen`], guarded by UIDVALIDITY.
    fn store_seen_checked(
        &mut self,
        mailbox_path: &str,
        expected_uid_validity: i64,
        uid: i64,
        seen: bool,
    ) -> Result<Guarded<()>, ProviderError> {
        let current = self.select(mailbox_path)?.uid_validity;
        if !uid_validity_matches(expected_uid_validity, current) {
            return Ok(Guarded::UidValidityChanged { current });
        }
        self.store_seen(mailbox_path, uid, seen)
            .map(Guarded::Applied)
    }

    /// [`Self::store_flagged`], guarded by UIDVALIDITY.
    fn store_flagged_checked(
        &mut self,
        mailbox_path: &str,
        expected_uid_validity: i64,
        uid: i64,
        flagged: bool,
    ) -> Result<Guarded<()>, ProviderError> {
        let current = self.select(mailbox_path)?.uid_validity;
        if !uid_validity_matches(expected_uid_validity, current) {
            return Ok(Guarded::UidValidityChanged { current });
        }
        self.store_flagged(mailbox_path, uid, flagged)
            .map(Guarded::Applied)
    }

    /// [`Self::store_keyword`], guarded by UIDVALIDITY.
    fn store_keyword_checked(
        &mut self,
        mailbox_path: &str,
        expected_uid_validity: i64,
        uid: i64,
        keyword: &str,
        add: bool,
    ) -> Result<Guarded<KeywordStoreOutcome>, ProviderError> {
        let current = self.select(mailbox_path)?.uid_validity;
        if !uid_validity_matches(expected_uid_validity, current) {
            return Ok(Guarded::UidValidityChanged { current });
        }
        self.store_keyword(mailbox_path, uid, keyword, add)
            .map(Guarded::Applied)
    }

    /// [`Self::move_message_checked`] that also reports where the message
    /// landed (UIDPLUS COPYUID) when the server says. The default reports
    /// nothing.
    fn move_message_reporting(
        &mut self,
        src_path: &str,
        expected_uid_validity: i64,
        uid: i64,
        message_id: &str,
        dest_path: &str,
    ) -> Result<Guarded<Option<MovedTo>>, ProviderError> {
        self.move_message_checked(src_path, expected_uid_validity, uid, message_id, dest_path)
            .map(|g| match g {
                Guarded::Applied(()) => Guarded::Applied(None),
                Guarded::UidValidityChanged { current } => Guarded::UidValidityChanged { current },
            })
    }

    /// [`Self::move_message`], guarded by the SOURCE mailbox's UIDVALIDITY.
    fn move_message_checked(
        &mut self,
        src_path: &str,
        expected_uid_validity: i64,
        uid: i64,
        message_id: &str,
        dest_path: &str,
    ) -> Result<Guarded<()>, ProviderError> {
        let current = self.select(src_path)?.uid_validity;
        if !uid_validity_matches(expected_uid_validity, current) {
            return Ok(Guarded::UidValidityChanged { current });
        }
        self.move_message(src_path, uid, message_id, dest_path)
            .map(Guarded::Applied)
    }

    /// [`Self::delete_uid`], guarded by UIDVALIDITY. A permanent delete by
    /// bare UID is never safe without it: after a reset the UID names some
    /// other message.
    fn delete_uid_checked(
        &mut self,
        mailbox_path: &str,
        expected_uid_validity: i64,
        uid: i64,
    ) -> Result<Guarded<()>, ProviderError> {
        let current = self.select(mailbox_path)?.uid_validity;
        if !uid_validity_matches(expected_uid_validity, current) {
            return Ok(Guarded::UidValidityChanged { current });
        }
        self.delete_uid(mailbox_path, uid).map(Guarded::Applied)
    }

    /// [`Self::expunge_mailbox`], guarded by UIDVALIDITY (Empty Trash and
    /// permanent delete). The caller used to SELECT, compare, and then call
    /// `expunge_mailbox`, which SELECTs again without comparing: a mailbox
    /// rebuilt between the two would have had the snapshot's UIDs, now naming
    /// other messages, permanently removed.
    fn expunge_mailbox_checked(
        &mut self,
        mailbox_path: &str,
        expected_uid_validity: i64,
        uids: &str,
    ) -> Result<Guarded<()>, ProviderError> {
        let current = self.select(mailbox_path)?.uid_validity;
        if !uid_validity_matches(expected_uid_validity, current) {
            return Ok(Guarded::UidValidityChanged { current });
        }
        self.expunge_mailbox(mailbox_path, uids)
            .map(Guarded::Applied)
    }

    /// [`Self::mark_seen_uids`], guarded by UIDVALIDITY (Mark All as Read),
    /// for the same reason as [`Self::expunge_mailbox_checked`].
    fn mark_seen_uids_checked(
        &mut self,
        mailbox_path: &str,
        expected_uid_validity: i64,
        uids: &str,
    ) -> Result<Guarded<()>, ProviderError> {
        let current = self.select(mailbox_path)?.uid_validity;
        if !uid_validity_matches(expected_uid_validity, current) {
            return Ok(Guarded::UidValidityChanged { current });
        }
        self.mark_seen_uids(mailbox_path, uids)
            .map(Guarded::Applied)
    }

    /// Every UID in `mailbox_path` whose RFC822 Message-ID is `message_id`
    /// (ascending), where [`Self::find_uid_by_message_id`] reports only the
    /// highest. Lets a retried server-draft save find the copy an earlier
    /// attempt already appended AND any further duplicates to clean up
    ///. Read-only. The default knows only what
    /// `find_uid_by_message_id` can tell it: at most one UID.
    fn find_uids_by_message_id(
        &mut self,
        mailbox_path: &str,
        message_id: &str,
    ) -> Result<MessageIdMatches, ProviderError> {
        let found = self.find_uid_by_message_id(mailbox_path, message_id)?;
        Ok(MessageIdMatches {
            uid_validity: found.map(|a| a.uid_validity).unwrap_or(0),
            uids: found.map(|a| a.uid).into_iter().collect(),
        })
    }
}

// ---------------------------------------------------------------------------
// AI backend abstraction
// ---------------------------------------------------------------------------

/// Error variants for AI provider operations.
///
/// Variants carry only typed metadata (model name, hostname, short reason) —
/// never prompt text, message subjects, or body content. The executor maps
/// these to `error_code` strings in the database.
#[derive(Debug)]
pub enum AiError {
    /// The endpoint refused the connection (not a private address, TLS on http,
    /// or any other validation failure from the egress guard).
    EndpointRefused(String),
    /// The requested model is not available at the endpoint.
    ModelMissing(String),
    /// A network-level failure (connect refused, DNS, I/O error).
    Network(String),
    /// The request timed out waiting for a response.
    Timeout,
    /// The backend returned an unexpected protocol response.
    Backend(String),
    /// Generation was cancelled by the caller returning `StreamControl::Cancel`.
    Cancelled,
}

impl fmt::Display for AiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AiError::EndpointRefused(m) => write!(f, "endpoint refused: {m}"),
            AiError::ModelMissing(m) => write!(f, "model missing: {m}"),
            AiError::Network(m) => write!(f, "network error: {m}"),
            AiError::Timeout => write!(f, "request timed out"),
            AiError::Backend(m) => write!(f, "backend error: {m}"),
            AiError::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for AiError {}

/// Whether to keep streaming or stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamControl {
    /// Keep processing — feed the next delta to the callback.
    Continue,
    /// Abort the stream immediately. The provider closes the connection and
    /// returns `Err(AiError::Cancelled)`.
    Cancel,
}

/// A generation request to the AI backend.
///
/// `Debug` is implemented manually to redact `system` and `user` — these
/// fields contain assembled email content (subject, sender, body) and must
/// NEVER appear in logs, error messages, or formatted output. Only the model
/// name and sizing parameters are safe to print.
#[derive(Clone)]
pub struct GenerateRequest {
    /// Model name as understood by the backend (e.g. "llama3.1:8b").
    pub model: String,
    /// System prompt (instruction layer). Contains privacy-sensitive text.
    pub system: String,
    /// User turn. Contains assembled context (subject, sender, body).
    pub user: String,
    /// KV-cache / context window size in tokens, if overriding the server default.
    pub num_ctx: Option<u32>,
    /// Maximum tokens to generate.
    pub max_output_tokens: Option<u32>,
}

impl fmt::Debug for GenerateRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GenerateRequest")
            .field("model", &self.model)
            .field("system", &format!("<redacted {} chars>", self.system.len()))
            .field("user", &format!("<redacted {} chars>", self.user.len()))
            .field("num_ctx", &self.num_ctx)
            .field("max_output_tokens", &self.max_output_tokens)
            .finish()
    }
}

/// Token-usage summary returned after a completed generation.
#[derive(Debug, Clone, Default)]
pub struct GenerateSummary {
    /// Prompt tokens consumed (input), if reported by the backend.
    pub tokens_in: Option<i64>,
    /// Generated tokens (output), if reported by the backend.
    pub tokens_out: Option<i64>,
}

/// Information about a model available at the backend.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    /// Model name (e.g. "llama3.1:8b").
    pub name: String,
    /// Model size on disk in bytes, if reported.
    pub size_bytes: Option<i64>,
}

/// Synchronous AI generation backend. Blocking: no Tokio required.
///
/// Implementors must be `Send` so the executor can move them across a
/// thread boundary (though in v1 everything runs on one thread).
///
/// `AiConfig` (in the Ollama crate) carries the endpoint parameters and
/// has a manual `Debug` implementation that redacts anything sensitive.
pub trait AiBackend: Send {
    /// List models available at this backend. Used to validate `model`
    /// before enqueuing and for the settings UI model picker.
    fn list_models(&self) -> Result<Vec<ModelInfo>, AiError>;

    /// Stream a generation, calling `on_delta` for each text chunk.
    ///
    /// `on_delta` returns `StreamControl::Cancel` to abort mid-stream —
    /// the implementation MUST drop the connection and return
    /// `Err(AiError::Cancelled)`.
    ///
    /// Returns `Ok(GenerateSummary)` on success (all chunks received and
    /// a `done:true` sentinel seen), or `Err(_)` on any failure.
    fn generate(
        &self,
        req: &GenerateRequest,
        on_delta: &mut dyn FnMut(&str) -> StreamControl,
    ) -> Result<GenerateSummary, AiError>;

    /// Compute an embedding vector for `text` using `model`.
    ///
    /// Single request/response (no streaming, no cancellation) — Ollama's
    /// `/api/embeddings` returns the whole vector in one reply. `text` is
    /// assembled fresh by the caller immediately before this call and is
    /// never logged; implementations must not echo it into `AiError` either.
    fn embed(&self, model: &str, text: &str) -> Result<Vec<f32>, AiError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_range_set_normalises_reversed_overlapping_adjacent_and_unsorted_ranges() {
        // IMAP allows "5:3" for 3:5; ranges may overlap, touch, or arrive in
        // any order across several VANISHED lines.
        let reversed = RangeInclusive::new(5, 3);
        let set = UidRangeSet::from_ranges([20..=30, reversed, 4..=6, 31..=31, 25..=28, 40..=40]);
        assert_eq!(set.ranges(), &[3..=6, 20..=31, 40..=40]);
        assert!(set.contains(3) && set.contains(6) && set.contains(31) && set.contains(40));
        assert!(!set.contains(2) && !set.contains(7) && !set.contains(32) && !set.contains(41));
        assert!(!set.is_empty());
        assert!(UidRangeSet::default().is_empty());
    }

    #[test]
    fn uid_range_set_keeps_the_whole_uid_space_as_one_range() {
        // A hostile or huge VANISHED range must stay O(ranges), never
        // O(UIDs): expanding this one would be 4 294 967 295 entries.
        let set = UidRangeSet::from_ranges(std::iter::once(1..=i64::from(u32::MAX)));
        assert_eq!(set.ranges().len(), 1);
        assert!(set.contains(1) && set.contains(i64::from(u32::MAX)));
        assert!(!set.contains(0) && !set.contains(i64::from(u32::MAX) + 1));
    }

    #[test]
    fn uid_range_set_collects_single_uids() {
        let set: UidRangeSet = [12, 10, 11, 42].into_iter().collect();
        assert_eq!(set.ranges(), &[10..=12, 42..=42]);
    }

    #[test]
    fn transport_security_resolves_the_stored_account_keys() {
        use TransportSecurity::*;
        assert_eq!(TransportSecurity::resolve(Some("tls"), false), ImplicitTls);
        assert_eq!(
            TransportSecurity::resolve(Some("starttls"), false),
            StartTls
        );
        assert_eq!(TransportSecurity::resolve(Some("STARTTLS"), true), StartTls);
        assert_eq!(TransportSecurity::resolve(Some("none"), true), Plaintext);
        assert_eq!(TransportSecurity::resolve(None, true), ImplicitTls);
        assert_eq!(TransportSecurity::resolve(None, false), Plaintext);
        // Unknown values fail closed (a TLS error, never cleartext).
        assert_eq!(TransportSecurity::resolve(Some("ssl3"), false), ImplicitTls);
        assert!(StartTls.is_encrypted() && ImplicitTls.is_encrypted());
        assert!(!Plaintext.is_encrypted());
    }

    #[test]
    fn imap_config_debug_redacts_password() {
        let cfg = ImapConfig {
            host: "imap.example.com".into(),
            port: 993,
            username: "alice".into(),
            password: "hunter2-super-secret".into(),
            security: TransportSecurity::ImplicitTls,
            allow_invalid_certs: false,
        };
        let s = format!("{cfg:?}");
        assert!(!s.contains("hunter2-super-secret"), "password leaked: {s}");
        assert!(s.contains("<redacted>"));
        // The non-secret fields must still be visible for debugging.
        assert!(s.contains("imap.example.com") && s.contains("alice") && s.contains("993"));
    }

    #[test]
    fn write_methods_default_to_unsupported() {
        // A provider that implements only the read surface must refuse the
        // write path cleanly (typed error), never silently succeed.
        struct ReadOnly;
        impl MailProvider for ReadOnly {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![])
            }
            fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
                Ok(MailboxState {
                    uid_validity: 1,
                    uid_next: 1,
                    exists: 0,
                })
            }
            fn fetch_new(
                &mut self,
                _after: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                Ok(vec![])
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }
        let mut p = ReadOnly;
        let err = p.append("Sent", "<id@x>", b"raw").unwrap_err();
        assert!(err.to_string().contains("append not supported"));

        let err = p.fetch_attachment_part(1, "2").unwrap_err();
        assert!(err
            .to_string()
            .contains("fetch_attachment_part not supported"));

        let err = p
            .append_with_flags_get_uid("Drafts", &[MessageFlag::Draft], b"raw")
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("append_with_flags_get_uid not supported"));

        let err = p.find_uid_by_message_id("Drafts", "<id@x>").unwrap_err();
        assert!(err
            .to_string()
            .contains("find_uid_by_message_id not supported"));

        let err = p.delete_uid("Drafts", 42).unwrap_err();
        assert!(err.to_string().contains("delete_uid not supported"));
    }

    #[test]
    fn store_keyword_defaults_to_unsupported() {
        struct ReadOnly;
        impl MailProvider for ReadOnly {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![])
            }
            fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
                Ok(MailboxState {
                    uid_validity: 1,
                    uid_next: 1,
                    exists: 0,
                })
            }
            fn fetch_new(
                &mut self,
                _after: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                Ok(vec![])
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }
        let mut p = ReadOnly;
        let err = p.store_keyword("INBOX", 1, "Work", true).unwrap_err();
        assert!(err.to_string().contains("store_keyword not supported"));
    }

    #[test]
    fn qresync_methods_default_to_unsupported_and_unavailable() {
        struct ReadOnly;
        impl MailProvider for ReadOnly {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![])
            }
            fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
                Ok(MailboxState {
                    uid_validity: 1,
                    uid_next: 1,
                    exists: 0,
                })
            }
            fn fetch_new(
                &mut self,
                _after: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                Ok(vec![])
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }
        let mut p = ReadOnly;
        assert!(!p.qresync_available());
        assert_eq!(p.last_highest_modseq(), None);
        let err = p.select_qresync("INBOX", 1, 100).unwrap_err();
        assert!(err.to_string().contains("select_qresync not supported"));
    }

    #[test]
    fn generate_request_debug_redacts_system_and_user() {
        let sentinel = "super-secret-body-content-SENTINEL";
        let req = GenerateRequest {
            model: "llama3.1:8b".into(),
            system: format!("Summarise this: {sentinel}"),
            user: format!("From: alice\n\n{sentinel}"),
            num_ctx: Some(8192),
            max_output_tokens: Some(1024),
        };
        let debug_str = format!("{req:?}");
        assert!(
            !debug_str.contains(sentinel),
            "sentinel leaked into Debug output: {debug_str}"
        );
        // Non-sensitive fields must still be visible for diagnostics.
        assert!(
            debug_str.contains("llama3.1:8b"),
            "model missing from Debug"
        );
        assert!(debug_str.contains("8192"), "num_ctx missing from Debug");
        assert!(debug_str.contains("<redacted"), "redaction marker missing");
    }

    #[test]
    fn smtp_config_debug_redacts_password() {
        let cfg = SmtpConfig {
            host: "smtp.example.com".into(),
            port: 587,
            username: "alice".into(),
            password: "hunter2-super-secret".into(),
            use_tls: true,
            allow_invalid_certs: false,
        };
        let s = format!("{cfg:?}");
        assert!(!s.contains("hunter2-super-secret"), "password leaked: {s}");
        assert!(s.contains("<redacted>"));
        assert!(s.contains("smtp.example.com") && s.contains("587"));
    }

    #[test]
    fn idle_methods_default_to_unsupported_and_unavailable() {
        struct ReadOnly;
        impl MailProvider for ReadOnly {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![])
            }
            fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
                Ok(MailboxState {
                    uid_validity: 1,
                    uid_next: 1,
                    exists: 0,
                })
            }
            fn fetch_new(
                &mut self,
                _after: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                Ok(vec![])
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }
        let mut p = ReadOnly;
        assert!(!p.idle_supported());
        assert!(p.idle_start("INBOX").is_err());
        assert!(p.idle_wait(std::time::Duration::from_secs(1)).is_err());
        assert!(p.idle_done().is_err());
    }
}

#[cfg(test)]
mod message_id_match_tests {
    use super::message_ids_match;

    #[test]
    fn exact_ids_match_ignoring_brackets_whitespace_and_domain_case() {
        assert!(message_ids_match("<a@x.test>", "a@x.test"));
        assert!(message_ids_match(" <a@X.TEST> ", "<a@x.test>"));
    }

    #[test]
    fn substrings_and_other_local_parts_do_not_match() {
        assert!(!message_ids_match("<xa@x.test>", "<a@x.test>"));
        assert!(!message_ids_match("<a@x.test.evil>", "<a@x.test>"));
        assert!(
            !message_ids_match("<A@x.test>", "<a@x.test>"),
            "local part is case-sensitive"
        );
        assert!(!message_ids_match("", ""));
        assert!(!message_ids_match("<>", "<a@x.test>"));
    }
}
