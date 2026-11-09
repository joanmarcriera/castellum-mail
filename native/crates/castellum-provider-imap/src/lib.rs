// Castellum - IMAP mail provider (blocking).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Implements `castellum_providers::MailProvider` as a thin adapter over
// `session::Session`, the imap-codec/imap-types backed IMAP session. TLS
// validation is ON by default (native-tls); invalid certs are only accepted
// when the config explicitly opts in (local test profiles). Fetches use
// BODY.PEEK[] so the remote \Seen flag is never modified by a read.
//
// History: this crate used to sit on the `imap`/`imap-proto` crates, whose
// parser had no NAMESPACE (RFC 2342) support. The 2026-07-11 Gmail pilot sent
// NAMESPACE, got an unparseable response, stranded the tagged OK on the socket
// and panicked on the next command (`fa8d5731`). `imap-codec` parses NAMESPACE
// and returns a typed error for anything it cannot decode, and `Session`
// poisons itself on the first such failure, so NAMESPACE is sent again (see
// `ImapProvider::namespaces`). `PanicGuardedProvider` still wraps
// this provider as defence in depth.
// See project-docs/imap-crate-migration-2026-09-17.md.

use castellum_providers::{
    uid_validity_matches, AppendedUid, DescriptorChunkResult, DescriptorFetchIncomplete,
    DescriptorFetchStop, DescriptorSkipReason, Guarded, IdleWake, ImapConfig, KeywordStoreOutcome,
    MailProvider, MailboxRoleSource, MailboxState, MessageFlag, MessageIdMatches, MovedTo,
    ProviderError, QresyncSync, RemoteAddress, RemoteMailbox, RemoteMessage,
    RemoteMessageDescriptor, SessionAbandonReason, SkippedDescriptor, TransportSecurity, UidFlags,
    UidRangeSet,
};
use session::{
    CopyUid, FetchItem, FetchedMessage, NamespaceResult, PermanentFlags, Session, SessionAddress,
    SessionError, SessionFlag, StoreSign,
};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};

/// Per-address TCP connect bound.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
/// Per-read/write socket bound; a silent server is declared dead after this.
const IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

pub mod encoded_words;
pub mod session;

/// Read-timeout control, needed only by `Session::idle_wait` (S31): idling
/// blocks on the socket for up to a bounded tick so the supervisor thread can
/// periodically check its shutdown/nudge channel without a second thread or
/// a self-pipe. Implemented per concrete stream type because neither
/// `TcpStream` nor `native_tls::TlsStream` share a common timeout trait.
pub trait ReadTimeout {
    fn set_read_timeout(&self, dur: Option<std::time::Duration>) -> std::io::Result<()>;
}

impl ReadTimeout for TcpStream {
    fn set_read_timeout(&self, dur: Option<std::time::Duration>) -> std::io::Result<()> {
        TcpStream::set_read_timeout(self, dur)
    }
}

impl ReadTimeout for native_tls::TlsStream<TcpStream> {
    fn set_read_timeout(&self, dur: Option<std::time::Duration>) -> std::io::Result<()> {
        self.get_ref().set_read_timeout(dur)
    }
}

/// `ImapProvider` stores its session as `Session<Box<dyn ReadWrite>>` (below)
/// regardless of transport, so the boxed trait object itself must forward
/// `ReadTimeout` too — unlike `Read`/`Write`, the standard library has no
/// built-in `impl<T: ReadTimeout + ?Sized> ReadTimeout for Box<T>` for a
/// crate-local trait.
impl<T: ReadTimeout + ?Sized> ReadTimeout for Box<T> {
    fn set_read_timeout(&self, dur: Option<std::time::Duration>) -> std::io::Result<()> {
        (**self).set_read_timeout(dur)
    }
}

/// Unifies the TLS and plaintext streams behind one boxed type so the session
/// has a single concrete type regardless of transport. `ReadTimeout` is a
/// supertrait so `Box<dyn ReadWrite>` (used generically as `Session<S>`'s `S`
/// in `ImapProvider`) can still call `set_read_timeout` through the trait
/// object — Rust lets a `dyn Trait` call its supertraits' methods directly.
trait ReadWrite: Read + Write + Send + ReadTimeout {}
impl<T: Read + Write + Send + ReadTimeout> ReadWrite for T {}

/// A connected, authenticated IMAP session.
pub struct ImapProvider {
    session: Session<Box<dyn ReadWrite>>,
    /// CAPABILITY tokens captured once at connect (empty = capture failed).
    cached_capabilities: Vec<String>,
    /// Set once at connect: `ENABLE CONDSTORE QRESYNC` succeeded AND the
    /// server's own `ENABLED` list actually named QRESYNC (a server could in
    /// principle ENABLE only CONDSTORE and ignore QRESYNC).
    qresync_enabled: bool,
    /// The most recent `select`/`select_qresync` call's HIGHESTMODSEQ, for
    /// `last_highest_modseq()`.
    last_highest_modseq: Option<i64>,
    /// Kept ONLY so descriptor isolation can transparently reconnect: a
    /// decode failure poisons `Session` permanently (CLAUDE.md gotcha 1),
    /// and unlike every other call this crate makes, the descriptor
    /// methods' contract forbids surfacing a per-message decode failure as
    /// an error — they must isolate the bad UID and keep going, which needs
    /// a fresh connection. No other method reconnects, and `cfg` is never
    /// logged (`ImapConfig`'s own `Debug` redacts `password`, mirrored by
    /// `ImapProvider`'s manual `Debug` below).
    cfg: ImapConfig,
    /// Reconnects this provider (= one sync pass) may still spend isolating
    /// undecodable messages during import, INCLUDING the reserve (see
    /// `MAX_DESCRIPTOR_RECONNECTS_PER_SYNC`). Never refilled.
    reconnects_remaining: u32,
    /// Reconnects the decode-skip retries may still spend this pass — a
    /// separate pool (see `MAX_RETRY_RECONNECTS_PER_SYNC`). Never refilled.
    retry_reconnects_remaining: u32,
    /// Set when a decode failure poisoned the session and it could not be
    /// replaced (no reconnect left, or the reconnect itself failed). The
    /// session is never used again; see `session_abandoned`.
    abandoned: bool,
}

/// Reconnects ONE sync pass (one `ImapProvider`; the sync engine opens one
/// per `sync_imap_json`) may spend isolating undecodable messages during
/// import, across every mailbox. Each reconnect is a full TCP/TLS + LOGIN,
/// so this is what keeps a decoder gap that hits every message (VANISHED and
/// the Gmail BODYSTRUCTURE bug each did once) from turning into a LOGIN storm
/// and real throttling/lockout.
///
/// Review of PR #180, round 2 (finding 1): the first bound (12, spent by a
/// binary bisection) let ONE bad UID consume the whole budget — 11
/// bisection nodes plus a confirm — so a second bad UID in the same
/// 1000-UID chunk stalled the mailbox on every pass. Isolation now costs 1
/// reconnect for the failing request plus 2 per bad UID (its 32-UID
/// sub-batch, then the UID alone; 1 when it shares a sub-batch already
/// being walked), so 16 isolates ~7 bad UIDs per pass.
const MAX_DESCRIPTOR_RECONNECTS_PER_SYNC: u32 = 16;

/// Of `MAX_DESCRIPTOR_RECONNECTS_PER_SYNC`, how many isolation must never
/// spend on continuing (review finding 2). When a decode failure leaves
/// only the reserve, isolation stops and spends it on a clean reconnect +
/// re-SELECT, returning what it covered with
/// `DescriptorFetchStop::ReconnectBudgetExhausted` — so the NEXT mailbox
/// starts on a clean session instead of failing at SELECT. With the reserve
/// also gone, a decode failure abandons the session explicitly
/// (`DescriptorFetchStop::SessionAbandoned`, `session_abandoned() == true`).
const RESERVED_CLEANUP_RECONNECTS: u32 = 1;

/// Reconnects per sync pass for `retry_descriptor_skips` — a pool of its
/// own (review finding 3: retries used to run BEFORE the import on the
/// shared budget, 2 LOGINs per permanently bad UID every pass, enough to
/// starve or poison the import). One still-bad UID costs exactly one.
const MAX_RETRY_RECONNECTS_PER_SYNC: u32 = 2;

/// UIDs per re-fetch when isolation walks a failed request: small enough
/// that walking a failing one UID at a time is cheap in round trips (no
/// reconnect unless a UID is bad), large enough that a 1000-UID chunk needs
/// only ~32 of them.
const ISOLATION_SUB_BATCH_UIDS: u64 = 32;

/// One descriptor request may skip at most `min(ceil(5% of its UIDs), 20)`
/// UIDs; the next undecodable UID ends the call just below itself
/// (`DescriptorFetchStop::SkipBudgetExceeded`), with everything before it
/// kept, so the next pass resumes AT that UID with a fresh allowance. It
/// binds for requests under ~140 UIDs (tail chunks, small mailboxes); for a
/// full 1000-UID chunk the reconnect budget (~7 UIDs per pass) binds first.
/// Either way a systemic decode failure advances a few UIDs per pass, each
/// recorded in the retry set, instead of skipping a whole mailbox in one go.
fn max_skips_per_request(uid_count: u64) -> usize {
    let n = usize::try_from(uid_count).unwrap_or(usize::MAX);
    n.saturating_mul(5).div_ceil(100).min(20)
}

/// A set of UIDs as sorted, disjoint, non-adjacent inclusive runs — the
/// shape one `UID FETCH` sequence set takes. Built by the provider itself so
/// a 1000-UID sparse group is `a:b,c,...` rather than every UID
/// comma-joined (review nit 5; imap-codec's default-on
/// `quirk_always_normalize_sequence_sets` also merges runs on encode, but
/// only after the long string was built and parsed, and only while that
/// feature stays on).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct UidRuns(Vec<(u32, u32)>);

impl UidRuns {
    /// Every valid UID (`1..=u32::MAX`) in `uids`, any order, duplicates ok.
    fn from_uids(uids: &[i64]) -> Self {
        let mut valid: Vec<u32> = uids
            .iter()
            .filter_map(|&u| u32::try_from(u).ok())
            .filter(|&u| u > 0)
            .collect();
        valid.sort_unstable();
        valid.dedup();
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for uid in valid {
            match runs.last_mut() {
                Some((_, end)) if end.checked_add(1) == Some(uid) => *end = uid,
                _ => runs.push((uid, uid)),
            }
        }
        UidRuns(runs)
    }

    /// `low..=high`, clamped to the valid UID space; empty when inverted.
    fn from_range(low: i64, high: i64) -> Self {
        let low = low.max(1);
        let high = high.min(i64::from(u32::MAX));
        if low > high {
            return UidRuns::default();
        }
        // Both bounds are within 1..=u32::MAX after the clamps above.
        UidRuns(vec![(low as u32, high as u32)])
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn len(&self) -> u64 {
        self.0.iter().map(|(a, b)| u64::from(b - a) + 1).sum()
    }

    fn last(&self) -> Option<i64> {
        self.0.last().map(|(_, b)| i64::from(*b))
    }

    fn contains(&self, uid: i64) -> bool {
        let Ok(uid) = u32::try_from(uid) else {
            return false;
        };
        let i = self.0.partition_point(|(_, end)| *end < uid);
        self.0.get(i).is_some_and(|(start, _)| *start <= uid)
    }

    /// Every UID, ascending. Only walked for one sub-batch (≤ 32 UIDs) or a
    /// retry list (≤ 20), never for a whole large range.
    fn uids(&self) -> impl Iterator<Item = i64> + '_ {
        self.0.iter().flat_map(|&(a, b)| (a..=b).map(i64::from))
    }

    /// Consecutive batches of at most `size` UIDs each, in order.
    fn batches(&self, size: u64) -> Vec<UidRuns> {
        let mut out: Vec<UidRuns> = Vec::new();
        let mut current: Vec<(u32, u32)> = Vec::new();
        let mut room = size;
        for &(mut start, end) in &self.0 {
            loop {
                let available = u64::from(end - start) + 1;
                let take = available.min(room);
                // `take >= 1` and `start + take - 1 <= end`, so no overflow.
                let stop = start + (take - 1) as u32;
                current.push((start, stop));
                room -= take;
                if room == 0 {
                    out.push(UidRuns(std::mem::take(&mut current)));
                    room = size;
                }
                if stop == end {
                    break;
                }
                start = stop + 1;
            }
        }
        if !current.is_empty() {
            out.push(UidRuns(current));
        }
        out
    }

    /// The IMAP sequence-set text: `a:b` per run, `a` for a single UID.
    fn as_set_string(&self) -> String {
        self.0
            .iter()
            .map(|&(a, b)| {
                if a == b {
                    a.to_string()
                } else {
                    format!("{a}:{b}")
                }
            })
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// How one descriptor `UID FETCH` ended when it did not hit a real error.
enum Fetched {
    Descriptors(Vec<RemoteMessageDescriptor>),
    /// The response could not be decoded, so `Session` is now poisoned
    /// (CLAUDE.md gotcha 1): only a new connection can send anything else.
    Undecodable,
}

fn stopped(
    mut out: DescriptorChunkResult,
    covered_through: Option<i64>,
    reason: DescriptorFetchStop,
) -> DescriptorChunkResult {
    out.incomplete = Some(DescriptorFetchIncomplete {
        covered_through,
        reason,
    });
    out
}

fn abandoned_error() -> ProviderError {
    ProviderError::Protocol(
        "imap session abandoned after an undecodable response with no reconnect left; \
         reconnect required"
            .into(),
    )
}

/// Descriptor-only FETCH items shared by `fetch_new_descriptors` and the
/// chunked descriptor methods: deliberately no `BODY.PEEK[]` — descriptor
/// import fetches metadata only, never a body.
const DESCRIPTOR_FETCH_ITEMS: &[FetchItem] = &[
    FetchItem::Uid,
    FetchItem::Flags,
    FetchItem::Rfc822Size,
    FetchItem::Envelope,
    FetchItem::InternalDate,
    FetchItem::BodyStructure,
];

/// Shared by `fetch_new_descriptors` and the chunked descriptor methods: one
/// `FetchedMessage` into the provider-agnostic descriptor shape. `None`
/// only if the server's FETCH response carried no UID (shouldn't happen —
/// `DESCRIPTOR_FETCH_ITEMS` always asks for one — but a missing UID can never
/// be safely attributed to any message, so it is dropped rather than guessed).
fn descriptor_from_fetched(f: FetchedMessage) -> Option<RemoteMessageDescriptor> {
    let uid = f.uid? as i64;
    let is_seen = f.flags.contains(&SessionFlag::Seen);
    let is_flagged = f.flags.contains(&SessionFlag::Flagged);
    let envelope = f.envelope;
    Some(RemoteMessageDescriptor {
        uid,
        size_bytes: f.size_bytes.map(|s| s as i64),
        has_attachments: f.has_attachments,
        is_seen,
        is_flagged,
        rfc822_message_id: envelope.as_ref().and_then(|e| e.message_id.clone()),
        subject: envelope.as_ref().and_then(|e| e.subject.clone()),
        from: envelope
            .as_ref()
            .and_then(|e| e.from.first())
            .map(remote_address),
        to: envelope
            .as_ref()
            .map(|e| remote_addresses(&e.to))
            .unwrap_or_default(),
        cc: envelope
            .as_ref()
            .map(|e| remote_addresses(&e.cc))
            .unwrap_or_default(),
        bcc: envelope
            .as_ref()
            .map(|e| remote_addresses(&e.bcc))
            .unwrap_or_default(),
        // One clock with body ingest (`castellum_mail::parse_message` reads the
        // Date header): prefer the ENVELOPE date, fall back to INTERNALDATE
        // only when the header is missing/unparseable. Mixing the two made rows
        // re-sort the moment a body hydrated (#3040).
        sent_at: envelope
            .as_ref()
            .and_then(|e| e.date_epoch)
            .or(f.internal_date_epoch),
        references: envelope.map(|e| e.in_reply_to).unwrap_or_default(),
        keywords: Some(f.keywords),
    })
}

/// `SessionError`'s only server-controlled free text is the `text` of a tagged
/// NO/BAD (the same trust boundary `ImapProvider` has always crossed).
fn session_err_to_provider_err(e: SessionError) -> ProviderError {
    match e {
        SessionError::Io(m) => ProviderError::Network(m),
        SessionError::Protocol(m) => ProviderError::Protocol(m),
        SessionError::No { command, text } | SessionError::Bad { command, text } => {
            ProviderError::Protocol(format!("{command}: {text}"))
        }
    }
}

/// RFC 9051 §7.1 (see plan Assumption 6): no PERMANENTFLAGS code at all means
/// the client should assume every flag — including any new keyword — can be
/// changed permanently; a code with `\*` allows any keyword; a code without
/// it allows only the keywords it lists (case-insensitive).
pub fn keyword_is_permanent(pf: Option<&PermanentFlags>, keyword: &str) -> bool {
    match pf {
        None => true,
        Some(pf) => {
            pf.allows_new_keywords || pf.keywords.iter().any(|k| k.eq_ignore_ascii_case(keyword))
        }
    }
}

/// Bridges the provider-agnostic write flag onto the wire-level `SessionFlag`.
fn message_flag_to_session(f: MessageFlag) -> SessionFlag {
    match f {
        MessageFlag::Seen => SessionFlag::Seen,
        MessageFlag::Draft => SessionFlag::Draft,
        MessageFlag::Flagged => SessionFlag::Flagged,
        MessageFlag::Answered => SessionFlag::Answered,
        MessageFlag::Deleted => SessionFlag::Deleted,
    }
}

/// Performs the TLS handshake on a connected socket (`tls_wrap`, or a
/// pass-through in the STARTTLS wire tests).
type TlsUpgrade = fn(&ImapConfig, TcpStream) -> Result<Box<dyn ReadWrite>, ProviderError>;

/// The production TLS handshake on a connected socket (implicit TLS, and
/// the upgrade after STARTTLS). Validation is on unless the account
/// explicitly allows invalid certificates.
fn tls_wrap(cfg: &ImapConfig, tcp: TcpStream) -> Result<Box<dyn ReadWrite>, ProviderError> {
    let connector = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(cfg.allow_invalid_certs)
        .danger_accept_invalid_hostnames(cfg.allow_invalid_certs)
        .build()
        .map_err(|e| ProviderError::Tls(e.to_string()))?;
    let tls = connector
        .connect(&cfg.host, tcp)
        .map_err(|e| ProviderError::Tls(e.to_string()))?;
    Ok(Box::new(tls))
}

impl ImapProvider {
    /// Connect and log in. Establishes TLS ourselves (so validation policy is
    /// explicit) and hands the stream to `Session`.
    pub fn connect(cfg: &ImapConfig) -> Result<Self, ProviderError> {
        Self::connect_with_timeouts(cfg, CONNECT_TIMEOUT, IO_TIMEOUT)
    }

    /// `connect` with explicit bounds: `connect_timeout` per resolved address,
    /// `io_timeout` applied to every socket read and write (per call, so a long
    /// FETCH that keeps delivering bytes is unaffected). A server that accepts
    /// and then stays silent fails with `ProviderError::Network` instead of
    /// hanging the sync thread.
    pub fn connect_with_timeouts(
        cfg: &ImapConfig,
        connect_timeout: std::time::Duration,
        io_timeout: std::time::Duration,
    ) -> Result<Self, ProviderError> {
        let net = |e: std::io::Error| ProviderError::Network(e.to_string());
        let mut last_err: Option<std::io::Error> = None;
        let mut tcp: Option<TcpStream> = None;
        for addr in (cfg.host.as_str(), cfg.port)
            .to_socket_addrs()
            .map_err(net)?
        {
            match TcpStream::connect_timeout(&addr, connect_timeout) {
                Ok(s) => {
                    tcp = Some(s);
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        let tcp = match tcp {
            Some(s) => s,
            None => {
                return Err(match last_err {
                    Some(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                        ProviderError::Network("timed out waiting for the server".into())
                    }
                    Some(e) => net(e),
                    None => ProviderError::Network("host did not resolve".into()),
                })
            }
        };
        tcp.set_read_timeout(Some(io_timeout)).map_err(net)?;
        tcp.set_write_timeout(Some(io_timeout)).map_err(net)?;
        Self::connect_on(cfg, tcp, io_timeout, tls_wrap)
    }

    /// Everything after the TCP connect: the security mode's handshake,
    /// LOGIN, CAPABILITY, ENABLE. `upgrade` performs the TLS handshake on the
    /// raw stream (a seam so the STARTTLS wire tests can run against a
    /// scripted cleartext server; production always passes `tls_wrap`).
    fn connect_on(
        cfg: &ImapConfig,
        tcp: TcpStream,
        io_timeout: std::time::Duration,
        upgrade: TlsUpgrade,
    ) -> Result<Self, ProviderError> {
        let mut session = match cfg.security {
            TransportSecurity::ImplicitTls => {
                Session::connect(upgrade(cfg, tcp)?).map_err(session_err_to_provider_err)?
            }
            TransportSecurity::StartTls => {
                // Mandatory: a refusal, an injected byte or a failed
                // handshake is a TLS error, and no LOGIN is ever sent.
                let plain = Session::connect(tcp).map_err(session_err_to_provider_err)?;
                let raw = plain.starttls().map_err(|e| {
                    ProviderError::Tls(format!("the server refused or broke STARTTLS: {e}"))
                })?;
                Session::resume_after_starttls(upgrade(cfg, raw)?)
            }
            TransportSecurity::Plaintext => {
                let stream: Box<dyn ReadWrite> = Box::new(tcp);
                Session::connect(stream).map_err(session_err_to_provider_err)?
            }
        };
        session.set_default_read_timeout(Some(io_timeout));
        session
            .login(&cfg.username, &cfg.password)
            .map_err(|e| match e {
                SessionError::No { text, .. } | SessionError::Bad { text, .. } => {
                    ProviderError::Auth(text)
                }
                other => session_err_to_provider_err(other),
            })?;
        // Capture CAPABILITY once while the session is idle, for diagnostics.
        // Failure is non-fatal: capabilities are informational only.
        let cached_capabilities = session.capability().unwrap_or_default();

        let supports_qresync = cached_capabilities
            .iter()
            .any(|c| c.eq_ignore_ascii_case("QRESYNC"))
            && cached_capabilities
                .iter()
                .any(|c| c.eq_ignore_ascii_case("CONDSTORE"));
        // ENABLE is idle-state-safe (RFC 5161); failure here is never fatal to
        // connect — an account whose server merely CLAIMS the capability but
        // rejects ENABLE still gets a working (non-incremental) account.
        let qresync_enabled = supports_qresync
            && session
                .enable(&["CONDSTORE", "QRESYNC"])
                .map(|enabled| enabled.iter().any(|c| c.eq_ignore_ascii_case("QRESYNC")))
                .unwrap_or(false);

        Ok(ImapProvider {
            session,
            cached_capabilities,
            qresync_enabled,
            last_highest_modseq: None,
            cfg: cfg.clone(),
            reconnects_remaining: MAX_DESCRIPTOR_RECONNECTS_PER_SYNC,
            retry_reconnects_remaining: MAX_RETRY_RECONNECTS_PER_SYNC,
            abandoned: false,
        })
    }

    /// Replace a poisoned session with a fresh one (new TCP/TLS, LOGIN,
    /// CAPABILITY, ENABLE) and re-SELECT `mailbox_path`, keeping `cfg` and
    /// both reconnect budgets (a fresh provider's full budgets would defeat
    /// them). The CALLER has already charged the reconnect to a budget.
    ///
    /// `Ok(false)`: the connect itself failed, so the poisoned session is
    /// still in place — marked abandoned, never used again. `Err`: the new
    /// session is clean but could not re-SELECT the mailbox.
    fn reconnect_and_reselect(&mut self, mailbox_path: &str) -> Result<bool, ProviderError> {
        let fresh = match Self::connect(&self.cfg) {
            Ok(fresh) => fresh,
            Err(_) => {
                self.abandoned = true;
                return Ok(false);
            }
        };
        self.session = fresh.session;
        self.cached_capabilities = fresh.cached_capabilities;
        self.qresync_enabled = fresh.qresync_enabled;
        self.last_highest_modseq = fresh.last_highest_modseq;
        // `mailbox_path`'s UIDVALIDITY is not re-verified here — the sync
        // engine already selected it for this pass and owns that comparison;
        // a reset mid-import (rare) is caught at the START of the next pass
        // by the ordinary `same_uid_space` check, which purges and refetches.
        self.session
            .select(mailbox_path)
            .map_err(session_err_to_provider_err)?;
        Ok(true)
    }

    /// After a decode failure during IMPORT isolation: spend one reconnect
    /// from the isolation budget to get a clean, re-selected session.
    ///
    /// `stop_with = None` means the caller wants to keep isolating: allowed
    /// only while more than the reserve is left (`Ok(None)`). Otherwise —
    /// the reserve is all that is left, or the caller is stopping anyway
    /// (`Some(reason)`) — the reconnect only leaves the session clean and
    /// the caller must stop with the returned reason. With no reconnect
    /// left at all, the session is abandoned (review finding 2: a poisoned
    /// session must never be left in place looking usable).
    fn recover_isolation(
        &mut self,
        mailbox_path: &str,
        stop_with: Option<DescriptorFetchStop>,
    ) -> Result<Option<DescriptorFetchStop>, ProviderError> {
        if self.reconnects_remaining == 0 {
            self.abandoned = true;
            return Ok(Some(DescriptorFetchStop::SessionAbandoned));
        }
        let keep_going =
            stop_with.is_none() && self.reconnects_remaining > RESERVED_CLEANUP_RECONNECTS;
        self.reconnects_remaining -= 1;
        if !self.reconnect_and_reselect(mailbox_path)? {
            return Ok(Some(DescriptorFetchStop::SessionAbandoned));
        }
        if keep_going {
            Ok(None)
        } else {
            Ok(Some(
                stop_with.unwrap_or(DescriptorFetchStop::ReconnectBudgetExhausted),
            ))
        }
    }

    /// One descriptor `UID FETCH` for exactly `runs`. Descriptors for UIDs
    /// outside `runs` (an unsolicited FETCH, or a server that ignored the
    /// set) are dropped: isolation must never credit a UID it did not ask
    /// for, or a checkpoint could pass one nothing durable recorded.
    fn fetch_runs(&mut self, runs: &UidRuns) -> Result<Fetched, ProviderError> {
        match self
            .session
            .uid_fetch(&runs.as_set_string(), DESCRIPTOR_FETCH_ITEMS)
        {
            Ok(fetched) => Ok(Fetched::Descriptors(
                fetched
                    .into_iter()
                    .filter_map(descriptor_from_fetched)
                    .filter(|d| runs.contains(d.uid))
                    .collect(),
            )),
            // Only a POISONING failure is a decode failure: a Protocol error
            // that left the session usable (e.g. an argument imap-types
            // rejected before anything was sent) is a real error.
            Err(SessionError::Protocol(_)) if self.session.is_poisoned() => {
                Ok(Fetched::Undecodable)
            }
            Err(e) => Err(session_err_to_provider_err(e)),
        }
    }

    /// The import path behind `fetch_descriptor_chunk`/`fetch_descriptor_list`:
    /// fetch `runs` in one `UID FETCH`; on a decode failure, reconnect and
    /// re-fetch in `ISOLATION_SUB_BATCH_UIDS`-sized sub-batches, fetching one
    /// UID at a time only inside a sub-batch that fails again (a request no
    /// bigger than one sub-batch goes straight to single UIDs). A single UID
    /// that fails alone, after its sub-batch failed on a fresh connection, is
    /// recorded as skipped; a one-off glitch that does not reproduce costs a
    /// reconnect and nothing else.
    ///
    /// Cost: 1 reconnect for the failing request, then 2 per bad UID (1 if
    /// it shares a sub-batch already being walked). When the budget runs
    /// down to its reserve, the per-request skip cap is reached, or any
    /// other failure interrupts isolation, this returns what it covered so
    /// far with `incomplete` set — ascending UID order means everything at
    /// or below `covered_through` is decoded, skipped, or absent, so the
    /// caller can commit it and checkpoint there. Only a failure of the
    /// very first, whole-request fetch (nothing covered, nothing spent) is
    /// an `Err`, exactly as a plain fetch would be.
    fn fetch_isolating(
        &mut self,
        mailbox_path: &str,
        runs: UidRuns,
    ) -> Result<DescriptorChunkResult, ProviderError> {
        let mut out = DescriptorChunkResult::default();
        if runs.is_empty() {
            return Ok(out);
        }
        if self.session_abandoned() {
            return Err(abandoned_error());
        }
        match self.fetch_runs(&runs)? {
            Fetched::Descriptors(d) => {
                out.descriptors = d;
                return Ok(out);
            }
            Fetched::Undecodable => {}
        }
        let mut covered: Option<i64> = None;
        let reason = match self.isolate(mailbox_path, &runs, &mut out, &mut covered) {
            Ok(None) => return Ok(out),
            Ok(Some(reason)) => reason,
            // Past this point work has been done (and reconnects spent) that
            // an `Err` would throw away: keep it.
            Err(_) => DescriptorFetchStop::Interrupted,
        };
        Ok(stopped(out, covered, reason))
    }

    /// The body of `fetch_isolating` once the whole-request fetch has
    /// failed to decode and poisoned the session. Appends to `out` and
    /// advances `covered` as it goes; `Ok(None)` = all of `runs` covered,
    /// `Ok(Some(reason))` = stopped on a budget, `Err` = interrupted by a
    /// non-decode failure (the caller keeps `out`/`covered` either way).
    fn isolate(
        &mut self,
        mailbox_path: &str,
        runs: &UidRuns,
        out: &mut DescriptorChunkResult,
        covered: &mut Option<i64>,
    ) -> Result<Option<DescriptorFetchStop>, ProviderError> {
        let max_skips = max_skips_per_request(runs.len());
        if let Some(reason) = self.recover_isolation(mailbox_path, None)? {
            return Ok(Some(reason));
        }
        // Re-fetching a request that is already a single sub-batch would
        // just fail again and waste a reconnect.
        let request_is_one_batch = runs.len() <= ISOLATION_SUB_BATCH_UIDS;
        for batch in runs.batches(ISOLATION_SUB_BATCH_UIDS) {
            if !request_is_one_batch {
                match self.fetch_runs(&batch)? {
                    Fetched::Descriptors(d) => {
                        out.descriptors.extend(d);
                        *covered = batch.last();
                        continue;
                    }
                    Fetched::Undecodable => {}
                }
                if let Some(reason) = self.recover_isolation(mailbox_path, None)? {
                    return Ok(Some(reason));
                }
            }
            for uid in batch.uids() {
                match self.fetch_runs(&UidRuns::from_range(uid, uid))? {
                    Fetched::Descriptors(d) => out.descriptors.extend(d),
                    Fetched::Undecodable if out.skipped.len() >= max_skips => {
                        // `uid` stays uncovered: the next pass resumes AT it
                        // with a fresh allowance. Still leave a clean session.
                        let reason = self
                            .recover_isolation(
                                mailbox_path,
                                Some(DescriptorFetchStop::SkipBudgetExceeded),
                            )?
                            .unwrap_or(DescriptorFetchStop::SkipBudgetExceeded);
                        return Ok(Some(reason));
                    }
                    Fetched::Undecodable => {
                        out.skipped.push(SkippedDescriptor {
                            uid,
                            reason: DescriptorSkipReason::Undecodable,
                        });
                        *covered = Some(uid);
                        if let Some(reason) = self.recover_isolation(mailbox_path, None)? {
                            return Ok(Some(reason));
                        }
                        continue;
                    }
                }
                *covered = Some(uid);
            }
        }
        Ok(None)
    }

    /// `retry_descriptor_skips`: each UID alone, on the retry budget only.
    /// A UID that still fails costs exactly one reconnect (made at once, so
    /// the session is always left clean); with no retry reconnect left, the
    /// remaining UIDs are not attempted at all — a fetch that might poison
    /// the session with nothing left to replace it is never sent. Any other
    /// failure stops with `Interrupted`, keeping what was already attempted.
    fn retry_isolating(
        &mut self,
        mailbox_path: &str,
        runs: UidRuns,
    ) -> Result<DescriptorChunkResult, ProviderError> {
        let mut out = DescriptorChunkResult::default();
        if runs.is_empty() {
            return Ok(out);
        }
        if self.session_abandoned() {
            return Err(abandoned_error());
        }
        let mut covered: Option<i64> = None;
        for uid in runs.uids() {
            if self.retry_reconnects_remaining == 0 {
                return Ok(stopped(
                    out,
                    covered,
                    DescriptorFetchStop::ReconnectBudgetExhausted,
                ));
            }
            match self.fetch_runs(&UidRuns::from_range(uid, uid)) {
                Ok(Fetched::Descriptors(d)) => out.descriptors.extend(d),
                Ok(Fetched::Undecodable) => {
                    out.skipped.push(SkippedDescriptor {
                        uid,
                        reason: DescriptorSkipReason::Undecodable,
                    });
                    self.retry_reconnects_remaining -= 1;
                    match self.reconnect_and_reselect(mailbox_path) {
                        Ok(true) => {}
                        Ok(false) => {
                            return Ok(stopped(
                                out,
                                Some(uid),
                                DescriptorFetchStop::SessionAbandoned,
                            ));
                        }
                        Err(_) => {
                            return Ok(stopped(out, Some(uid), DescriptorFetchStop::Interrupted));
                        }
                    }
                }
                Err(_) => return Ok(stopped(out, covered, DescriptorFetchStop::Interrupted)),
            }
            covered = Some(uid);
        }
        Ok(out)
    }

    /// RFC 2342: the server's NAMESPACE report (personal / other users /
    /// shared, each with its own delimiter). Used by `list_mailboxes` as a
    /// fallback for rows whose LIST delimiter is NIL (a row's own delimiter
    /// stays authoritative). Only sent when the server advertised NAMESPACE;
    /// any failure maps to `None`.
    fn namespaces(&mut self) -> Option<NamespaceResult> {
        let advertised = self
            .cached_capabilities
            .iter()
            .any(|c| c.eq_ignore_ascii_case("NAMESPACE"));
        if !advertised {
            return None;
        }
        self.session.namespace().ok()
    }
}

/// Manual `Debug`: `cached_capabilities` holds RAW server-controlled
/// CAPABILITY text (kept raw for protocol decisions; the diagnostics layer
/// allowlists it before persisting). A stray `{:?}` of a session in a log,
/// panic, or test failure must not echo server bytes.
impl std::fmt::Debug for ImapProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImapProvider")
            .field("session", &"<imap session>")
            .field(
                "cached_capabilities",
                &format_args!(
                    "<{} unsanitised tokens redacted>",
                    self.cached_capabilities.len()
                ),
            )
            .field("cfg", &self.cfg)
            .finish()
    }
}

/// Unwrap a write that ran with no UIDVALIDITY expectation. `select_for_write`
/// never refuses when given `None`, so the refusal arm is unreachable; it is
/// an error rather than a panic because this crate never panics on its wire
/// path.
fn unguarded<T>(outcome: Guarded<T>) -> Result<T, ProviderError> {
    match outcome {
        Guarded::Applied(value) => Ok(value),
        Guarded::UidValidityChanged { .. } => Err(ProviderError::Protocol(
            "unguarded write was refused on UIDVALIDITY".into(),
        )),
    }
}

/// The UIDVALIDITY-guarded write bodies. Each one
/// backs both a `MailProvider` write (`expected = None`: no guard, the
/// historical behaviour) and its `*_checked` twin (`expected = Some`): the
/// compare happens on the very SELECT the write needs, so there is no window
/// between the check and the command that acts on the UID.
impl ImapProvider {
    /// Narrow `SEARCH HEADER Message-ID` hits to the exact match. SEARCH HEADER
    /// is a SUBSTRING match ("<a@x>" also finds "<aa@x>" or a crafted
    /// "<a@x>.evil"), so each candidate's real Message-ID is fetched and
    /// compared before it counts. Every caller that acts on the result (Undo's
    /// move, the server-draft cleanup's STORE \Deleted + EXPUNGE, the recorded
    /// draft mirror UID) must go through this. Ascending, deduplicated. The
    /// mailbox must already be selected.
    fn exact_message_id_uids(
        &mut self,
        candidates: &[u32],
        message_id: &str,
    ) -> Result<Vec<i64>, ProviderError> {
        let proto = session_err_to_provider_err;
        let mut exact: Vec<i64> = Vec::new();
        for chunk in candidates.chunks(100) {
            let set = chunk
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let fetched = self
                .session
                .uid_fetch(&set, &[FetchItem::Uid, FetchItem::Envelope])
                .map_err(proto)?;
            for m in fetched {
                let header_id = m.envelope.as_ref().and_then(|e| e.message_id.as_deref());
                if let (Some(uid), Some(header_id)) = (m.uid, header_id) {
                    if castellum_providers::message_ids_match(header_id, message_id) {
                        exact.push(i64::from(uid));
                    }
                }
            }
        }
        exact.sort_unstable();
        exact.dedup();
        Ok(exact)
    }
}

impl ImapProvider {
    /// SELECT `path` (read-write). With `expected`, returns `Some(current)`
    /// when the mailbox's UIDVALIDITY is not that value, or is unknown on
    /// either side: the caller must then send nothing addressed by UID.
    fn select_for_write(
        &mut self,
        path: &str,
        expected: Option<i64>,
    ) -> Result<Option<i64>, ProviderError> {
        let mb = self
            .session
            .select(path)
            .map_err(session_err_to_provider_err)?;
        let Some(expected) = expected else {
            return Ok(None);
        };
        // -1 = the server reported no UIDVALIDITY (same convention as `select`).
        let current = mb.uid_validity.map(|v| v as i64).unwrap_or(-1);
        Ok((!uid_validity_matches(expected, current)).then_some(current))
    }

    fn store_flag_guarded(
        &mut self,
        mailbox_path: &str,
        expected: Option<i64>,
        uid: i64,
        flag: SessionFlag,
        set: bool,
    ) -> Result<Guarded<()>, ProviderError> {
        // SELECT (read-write) is required before UID STORE. It also clears the
        // server's \Recent on first access — a benign, standard side effect. We
        // re-select on every call so back-to-back ops across mailboxes are correct.
        if let Some(current) = self.select_for_write(mailbox_path, expected)? {
            return Ok(Guarded::UidValidityChanged { current });
        }
        let sign = if set {
            StoreSign::Add
        } else {
            StoreSign::Remove
        };
        self.session
            .uid_store(&uid.to_string(), sign, &[flag])
            .map_err(session_err_to_provider_err)?;
        Ok(Guarded::Applied(()))
    }

    fn store_keyword_guarded(
        &mut self,
        mailbox_path: &str,
        expected: Option<i64>,
        uid: i64,
        keyword: &str,
        add: bool,
    ) -> Result<Guarded<KeywordStoreOutcome>, ProviderError> {
        // SELECT first (read-write, same as store_flagged); its
        // PERMANENTFLAGS decide whether a STORE would persist at all.
        if let Some(current) = self.select_for_write(mailbox_path, expected)? {
            return Ok(Guarded::UidValidityChanged { current });
        }
        if !keyword_is_permanent(self.session.last_permanent_flags(), keyword) {
            return Ok(Guarded::Applied(KeywordStoreOutcome::NotPermanent));
        }
        let sign = if add {
            StoreSign::Add
        } else {
            StoreSign::Remove
        };
        self.session
            .uid_store_keywords(&uid.to_string(), sign, &[keyword])
            .map_err(session_err_to_provider_err)?;
        Ok(Guarded::Applied(KeywordStoreOutcome::Stored))
    }

    fn move_message_guarded(
        &mut self,
        src_path: &str,
        expected: Option<i64>,
        uid: i64,
        dest_path: &str,
    ) -> Result<Guarded<Option<MovedTo>>, ProviderError> {
        let proto = session_err_to_provider_err;
        let uid32 = u32::try_from(uid)
            .map_err(|_| ProviderError::Protocol(format!("invalid uid {uid}")))?;

        // Idempotency keyed on the SOURCE, not the destination: if the source UID
        // is already gone, a prior attempt completed the move — do nothing. This
        // never re-copies (no duplicate) and never trusts a Message-ID match
        // (which could point at a *different* message that happens to share an id,
        // and get it deleted). Worst case on a torn retry is a rare duplicate in
        // the destination — recoverable, unlike a deletion.
        if let Some(current) = self.select_for_write(src_path, expected)? {
            return Ok(Guarded::UidValidityChanged { current });
        }
        let present = !self
            .session
            .uid_search_by_uid(uid32)
            .map_err(proto)?
            .is_empty();
        if !present {
            return Ok(Guarded::Applied(None));
        }

        // Ensure the destination exists (auto-create Trash/Archive on servers that
        // lack it). CREATE on an existing mailbox errors harmlessly — ignore it.
        // A mailbox we just created is also subscribed: sync skips unsubscribed
        // archive mailboxes, so otherwise archived mail would vanish (#3017).
        if self.session.create(dest_path).is_ok() {
            let _ = self.session.subscribe(dest_path);
        }

        // Choose a safe primitive by capability. Never fall back to a full
        // EXPUNGE, which would destroy *other* \Deleted messages in the source.
        let caps = self.session.capability().map_err(proto)?;
        let has_move = caps.iter().any(|c| c.eq_ignore_ascii_case("MOVE"));
        let has_uidplus = caps.iter().any(|c| c.eq_ignore_ascii_case("UIDPLUS"));
        if !has_move && !has_uidplus {
            // Neither MOVE nor UIDPLUS: there is no way to remove a single message
            // without risking other \Deleted messages. Refuse (surfaced in the outbox)
            // rather than duplicate or over-delete.
            return Err(ProviderError::Protocol(
                "server supports neither MOVE nor UIDPLUS; cannot move a message safely".into(),
            ));
        }
        // The re-SELECT is the one the mutating commands run under, so it is
        // guarded again: the compare above covers the presence search, this
        // one covers the move itself.
        if let Some(current) = self.select_for_write(src_path, expected)? {
            return Ok(Guarded::UidValidityChanged { current });
        }
        if has_move {
            // Atomic server-side move (RFC 6851): no intermediate state.
            let landed = self
                .session
                .uid_move(&uid.to_string(), dest_path)
                .map_err(proto)?;
            return Ok(Guarded::Applied(moved_to(landed)));
        }
        // COPY + \Deleted + UID EXPUNGE (RFC 4315) expunges only this UID.
        let landed = self
            .session
            .uid_copy(&uid.to_string(), dest_path)
            .map_err(proto)?;
        self.session
            .uid_store(&uid.to_string(), StoreSign::Add, &[SessionFlag::Deleted])
            .map_err(proto)?;
        self.session.uid_expunge(&uid.to_string()).map_err(proto)?;
        Ok(Guarded::Applied(moved_to(landed)))
    }

    fn delete_uid_guarded(
        &mut self,
        mailbox_path: &str,
        expected: Option<i64>,
        uid: i64,
    ) -> Result<Guarded<()>, ProviderError> {
        let proto = session_err_to_provider_err;
        if let Some(current) = self.select_for_write(mailbox_path, expected)? {
            return Ok(Guarded::UidValidityChanged { current });
        }
        let set = uid.to_string();
        self.session
            .uid_store(&set, StoreSign::Add, &[SessionFlag::Deleted])
            .map_err(proto)?;
        // UID EXPUNGE (RFC 4315) scopes the expunge to exactly this UID; a
        // server without UIDPLUS only offers EXPUNGE, which removes every
        // \Deleted message in the mailbox (accepted trade-off for a Drafts
        // mailbox - see project-docs/plans/2026-09-18-server-drafts-and-microsoft-oauth.md
        // "Assumptions to confirm" #2).
        let has_uidplus = self
            .cached_capabilities
            .iter()
            .any(|c| c.eq_ignore_ascii_case("UIDPLUS"));
        if has_uidplus {
            self.session.uid_expunge(&set).map_err(proto)?;
        } else {
            self.session.expunge().map_err(proto)?;
        }
        Ok(Guarded::Applied(()))
    }
}

/// Bulk UID-set writes, guarded like the single-UID ones above (the tracker
/// #3472 follow-up to #3368): the compare rides on the SELECT the write
/// itself needs, so a mailbox rebuilt between a caller's check and this call
/// can no longer have the snapshot's UIDs expunged or marked read.
impl ImapProvider {
    fn mark_seen_uids_guarded(
        &mut self,
        mailbox_path: &str,
        expected: Option<i64>,
        uids: &str,
    ) -> Result<Guarded<()>, ProviderError> {
        if let Some(current) = self.select_for_write(mailbox_path, expected)? {
            return Ok(Guarded::UidValidityChanged { current });
        }
        // `uids` is the caller's snapshot (see the trait doc) — never "1:*".
        self.session
            .uid_store(uids, StoreSign::Add, &[SessionFlag::Seen])
            .map_err(session_err_to_provider_err)?;
        Ok(Guarded::Applied(()))
    }

    fn expunge_mailbox_guarded(
        &mut self,
        mailbox_path: &str,
        expected: Option<i64>,
        uids: &str,
    ) -> Result<Guarded<()>, ProviderError> {
        let proto = session_err_to_provider_err;
        if let Some(current) = self.select_for_write(mailbox_path, expected)? {
            return Ok(Guarded::UidValidityChanged { current });
        }
        // UIDPLUS-only (PR #165 review point 3): a plain EXPUNGE is the only
        // primitive available without it, and it removes EVERY \Deleted
        // message in the mailbox, not just the snapshot — an unacceptable
        // residual for a destructive op. Refuse before flagging anything,
        // exactly like `move_message`'s own MOVE/UIDPLUS refusal above.
        let caps = self.session.capability().map_err(proto)?;
        let has_uidplus = caps.iter().any(|c| c.eq_ignore_ascii_case("UIDPLUS"));
        if !has_uidplus {
            return Err(ProviderError::Unsupported(
                "server does not support UIDPLUS; cannot expunge Trash safely".into(),
            ));
        }
        // `uids` is the caller's pre-verified snapshot (see the trait doc) —
        // never "1:*". Only these UIDs are ever flagged \Deleted, so a message
        // that landed in Trash after the snapshot (another client, a
        // server-side rule, a delayed replay) is never touched here.
        self.session
            .uid_store(uids, StoreSign::Add, &[SessionFlag::Deleted])
            .map_err(proto)?;
        // RFC 4315: expunges exactly this UID set, leaving any OTHER
        // \Deleted message in Trash (flagged by another client) alone.
        self.session.uid_expunge(uids).map_err(proto)?;
        Ok(Guarded::Applied(()))
    }
}

impl MailProvider for ImapProvider {
    fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
        let subscribed_paths: HashSet<String> = self
            .session
            .lsub("", "*")
            .map_err(session_err_to_provider_err)?
            .into_iter()
            .map(|item| item.path)
            .collect();
        let items = self
            .session
            .list("", "*")
            .map_err(session_err_to_provider_err)?;
        // Delimiter fallbacks for a row that reports NIL: the delimiter of the
        // NAMESPACE entry (personal / other / shared) owning the path by
        // longest prefix, then the personal delimiter, then the first
        // delimiter any other row in the same LIST reported. A server that
        // reports NIL everywhere genuinely has a flat namespace.
        let namespaces = self.namespaces();
        let personal_delimiter = namespaces
            .as_ref()
            .and_then(|n| n.personal_delimiter.clone());
        let listed_delimiter = items.iter().find_map(|m| m.delimiter.clone());
        Ok(items
            .into_iter()
            .map(|item| {
                // Prefer RFC 6154 SPECIAL-USE attributes (reliable across
                // servers, e.g. iCloud's "Deleted Messages"); fall back to the
                // name.
                let (role, role_source) = role_with_source(&item.special_use, &item.path);
                // `no_select` covers `\Noselect` and `\NonExistent`: a
                // container-only node that errors on SELECT, so sync skips
                // fetching it (but still records it).
                let delimiter = item
                    .delimiter
                    .clone()
                    .or_else(|| {
                        namespaces
                            .as_ref()
                            .and_then(|n| n.delimiter_for(&item.path))
                    })
                    .or_else(|| personal_delimiter.clone())
                    .or_else(|| listed_delimiter.clone());
                RemoteMailbox {
                    role,
                    role_source,
                    name: leaf_name(&item.path, delimiter.as_deref()),
                    subscribed: subscribed_paths.contains(&item.path),
                    selectable: !item.no_select,
                    path: item.path,
                    delimiter,
                }
            })
            .collect())
    }

    fn select(&mut self, path: &str) -> Result<MailboxState, ProviderError> {
        let mb = self
            .session
            .select(path)
            .map_err(session_err_to_provider_err)?;
        self.last_highest_modseq = mb.highest_modseq.map(|v| v as i64);
        Ok(MailboxState {
            // -1 = server didn't report UIDVALIDITY → treat as unknown (full refetch).
            uid_validity: mb.uid_validity.map(|v| v as i64).unwrap_or(-1),
            uid_next: mb.uid_next.map(|v| v as i64).unwrap_or(0),
            exists: mb.exists as i64,
        })
    }

    fn fetch_new(&mut self, after_uid: Option<i64>) -> Result<Vec<RemoteMessage>, ProviderError> {
        let start = after_uid.map(|u| u + 1).unwrap_or(1).max(1);
        // "start:*" always returns at least the highest message even when none
        // are >= start, so we filter below.
        let set = format!("{start}:*");
        let fetched = self
            .session
            .uid_fetch(
                &set,
                &[FetchItem::Uid, FetchItem::Flags, FetchItem::BodyPeek],
            )
            .map_err(session_err_to_provider_err)?;
        Ok(fetched
            .into_iter()
            .filter_map(|f| remote_message_from_fetched(f, after_uid))
            .collect())
    }

    fn fetch_bodies(&mut self, uids: &[i64]) -> Result<Vec<RemoteMessage>, ProviderError> {
        if uids.is_empty() {
            return Ok(vec![]);
        }
        let set = uids
            .iter()
            .filter(|uid| **uid > 0)
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        if set.is_empty() {
            return Ok(vec![]);
        }
        let wanted: HashSet<i64> = uids.iter().copied().collect();
        let fetched = self
            .session
            .uid_fetch(
                &set,
                &[FetchItem::Uid, FetchItem::Flags, FetchItem::BodyPeek],
            )
            .map_err(session_err_to_provider_err)?;
        Ok(fetched
            .into_iter()
            .filter(|f| f.uid.is_some_and(|u| wanted.contains(&(u as i64))))
            .filter_map(|f| remote_message_from_fetched(f, None))
            .collect())
    }

    fn fetch_attachment_part(&mut self, uid: i64, section: &str) -> Result<Vec<u8>, ProviderError> {
        let path = parse_section_path(section)?;
        let uid32 = u32::try_from(uid)
            .map_err(|_| ProviderError::Protocol(format!("invalid uid {uid}")))?;
        self.session
            .uid_fetch_section(uid32, &path)
            .map_err(session_err_to_provider_err)?
            .ok_or_else(|| {
                ProviderError::Protocol(format!(
                    "server returned no data for uid {uid} section {section}"
                ))
            })
    }

    fn fetch_new_descriptors(
        &mut self,
        after_uid: Option<i64>,
    ) -> Result<Vec<RemoteMessageDescriptor>, ProviderError> {
        let start = after_uid.map(|u| u + 1).unwrap_or(1).max(1);
        // "start:*" always returns at least the highest message even when none
        // are >= start, so we filter below. This query deliberately avoids
        // BODY.PEEK[] and fetches only descriptor metadata.
        let set = format!("{start}:*");
        let fetched = self
            .session
            .uid_fetch(&set, DESCRIPTOR_FETCH_ITEMS)
            .map_err(session_err_to_provider_err)?;
        Ok(fetched
            .into_iter()
            .filter_map(descriptor_from_fetched)
            .filter(|d| after_uid.is_none_or(|after| d.uid > after))
            .collect())
    }

    fn fetch_descriptor_chunk(
        &mut self,
        mailbox_path: &str,
        low: i64,
        high: i64,
    ) -> Result<DescriptorChunkResult, ProviderError> {
        self.fetch_isolating(mailbox_path, UidRuns::from_range(low, high))
    }

    fn search_uids_in(&mut self, low: i64, high: i64) -> Result<Vec<i64>, ProviderError> {
        let low = low.max(1);
        let high = high.min(i64::from(u32::MAX));
        if low > high {
            return Ok(Vec::new());
        }
        // Both bounds are within 1..=u32::MAX after the clamps above.
        let mut uids: Vec<i64> = self
            .session
            .uid_search_uid_range(low as u32, high as u32)
            .map_err(session_err_to_provider_err)?
            .into_iter()
            .map(i64::from)
            // RFC 3501 §6.4.8's `n:*` quirk, or any server naming a UID
            // outside what was asked: never hand it back (nit 4).
            .filter(|uid| (low..=high).contains(uid))
            .collect();
        uids.sort_unstable();
        uids.dedup();
        Ok(uids)
    }

    fn fetch_descriptor_list(
        &mut self,
        mailbox_path: &str,
        uids: &[i64],
    ) -> Result<DescriptorChunkResult, ProviderError> {
        self.fetch_isolating(mailbox_path, UidRuns::from_uids(uids))
    }

    fn retry_descriptor_skips(
        &mut self,
        mailbox_path: &str,
        uids: &[i64],
    ) -> Result<DescriptorChunkResult, ProviderError> {
        self.retry_isolating(mailbox_path, UidRuns::from_uids(uids))
    }

    /// Explicitly abandoned by descriptor isolation, OR poisoned by an
    /// undecodable response from any other call (nothing but isolation
    /// ever replaces a poisoned session), OR closed (an I/O error or a
    /// server BYE mid-command). Either way no command can be sent again,
    /// and the sync engine must end the pass rather than fail every
    /// remaining mailbox at SELECT (review of PR #180, finding 2).
    fn session_abandoned(&self) -> bool {
        self.abandoned || self.session.is_poisoned() || self.session.is_closed()
    }

    ///: distinguishes a decode failure nothing ever tried to
    /// recover from (`Poisoned`, still "protocol") from the two
    /// connection-shaped causes that should read as "network" instead — a
    /// closed connection (`Closed`), or isolation explicitly abandoning the
    /// session (`BudgetExhausted`: `self.abandoned` is set only when the
    /// reconnect budget was already spent, or the reconnect it spent itself
    /// failed to reconnect). `self.abandoned` is checked first because both
    /// of its call sites leave the OLD, still-poisoned session in place
    /// (nothing replaces it on failure) — `is_poisoned()` would otherwise
    /// also be true and this would misreport `Poisoned`.
    fn session_abandon_reason(&self) -> Option<SessionAbandonReason> {
        if self.abandoned {
            Some(SessionAbandonReason::BudgetExhausted)
        } else if self.session.is_closed() {
            Some(SessionAbandonReason::Closed)
        } else if self.session.is_poisoned() {
            Some(SessionAbandonReason::Poisoned)
        } else {
            None
        }
    }

    fn first_uid_since(&mut self, imap_date: &str) -> Result<Option<i64>, ProviderError> {
        // `imap_date` is built by the sync engine (`imap_since_date`) from a
        // validated YYYY-MM-DD, always in RFC 3501 wire form ("1-Jun-2026").
        let date = chrono::NaiveDate::parse_from_str(imap_date, "%d-%b-%Y")
            .map_err(|e| ProviderError::Protocol(format!("bad SINCE date {imap_date:?}: {e}")))?;
        let uids = self
            .session
            .uid_search_since(date)
            .map_err(|e| ProviderError::Protocol(e.to_string()))?;
        Ok(uids.into_iter().min().map(i64::from))
    }

    fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
        // Descriptor-only fetch: FLAGS, no BODY.PEEK[], so it stays cheap even on
        // large mailboxes and never touches \Seen. "1:*" is the full UID range;
        // the caller (sync) must already have SELECTed a non-empty mailbox.
        let fetched = self
            .session
            .uid_fetch("1:*", &[FetchItem::Uid, FetchItem::Flags])
            .map_err(session_err_to_provider_err)?;
        Ok(fetched
            .into_iter()
            .filter_map(|f| {
                let uid = f.uid? as i64;
                Some(UidFlags {
                    uid,
                    is_seen: f.flags.contains(&SessionFlag::Seen),
                    is_flagged: f.flags.contains(&SessionFlag::Flagged),
                    keywords: Some(f.keywords),
                })
            })
            .collect())
    }

    fn capabilities(&self) -> Option<Vec<String>> {
        if self.cached_capabilities.is_empty() {
            None
        } else {
            Some(self.cached_capabilities.clone())
        }
    }

    fn logout(&mut self) -> Result<(), ProviderError> {
        self.session.logout().map_err(session_err_to_provider_err)
    }

    fn store_seen(
        &mut self,
        mailbox_path: &str,
        uid: i64,
        seen: bool,
    ) -> Result<(), ProviderError> {
        self.store_flag_guarded(mailbox_path, None, uid, SessionFlag::Seen, seen)
            .and_then(unguarded)
    }

    fn store_seen_checked(
        &mut self,
        mailbox_path: &str,
        expected_uid_validity: i64,
        uid: i64,
        seen: bool,
    ) -> Result<Guarded<()>, ProviderError> {
        self.store_flag_guarded(
            mailbox_path,
            Some(expected_uid_validity),
            uid,
            SessionFlag::Seen,
            seen,
        )
    }

    fn mark_all_seen(&mut self, mailbox_path: &str) -> Result<(), ProviderError> {
        self.session
            .select(mailbox_path)
            .map_err(session_err_to_provider_err)?;
        // 1:* is the full UID range; +FLAGS (\Seen) is idempotent.
        self.session
            .uid_store("1:*", StoreSign::Add, &[SessionFlag::Seen])
            .map_err(session_err_to_provider_err)
    }

    fn mark_seen_uids(&mut self, mailbox_path: &str, uids: &str) -> Result<(), ProviderError> {
        self.mark_seen_uids_guarded(mailbox_path, None, uids)
            .and_then(unguarded)
    }

    fn mark_seen_uids_checked(
        &mut self,
        mailbox_path: &str,
        expected_uid_validity: i64,
        uids: &str,
    ) -> Result<Guarded<()>, ProviderError> {
        self.mark_seen_uids_guarded(mailbox_path, Some(expected_uid_validity), uids)
    }

    fn store_flagged(
        &mut self,
        mailbox_path: &str,
        uid: i64,
        flagged: bool,
    ) -> Result<(), ProviderError> {
        self.store_flag_guarded(mailbox_path, None, uid, SessionFlag::Flagged, flagged)
            .and_then(unguarded)
    }

    fn store_flagged_checked(
        &mut self,
        mailbox_path: &str,
        expected_uid_validity: i64,
        uid: i64,
        flagged: bool,
    ) -> Result<Guarded<()>, ProviderError> {
        self.store_flag_guarded(
            mailbox_path,
            Some(expected_uid_validity),
            uid,
            SessionFlag::Flagged,
            flagged,
        )
    }

    fn store_keyword(
        &mut self,
        mailbox_path: &str,
        uid: i64,
        keyword: &str,
        add: bool,
    ) -> Result<KeywordStoreOutcome, ProviderError> {
        self.store_keyword_guarded(mailbox_path, None, uid, keyword, add)
            .and_then(unguarded)
    }

    fn store_keyword_checked(
        &mut self,
        mailbox_path: &str,
        expected_uid_validity: i64,
        uid: i64,
        keyword: &str,
        add: bool,
    ) -> Result<Guarded<KeywordStoreOutcome>, ProviderError> {
        self.store_keyword_guarded(mailbox_path, Some(expected_uid_validity), uid, keyword, add)
    }

    fn move_message(
        &mut self,
        src_path: &str,
        uid: i64,
        _message_id: &str,
        dest_path: &str,
    ) -> Result<(), ProviderError> {
        self.move_message_guarded(src_path, None, uid, dest_path)
            .map(|g| match g {
                Guarded::Applied(_) => Guarded::Applied(()),
                Guarded::UidValidityChanged { current } => Guarded::UidValidityChanged { current },
            })
            .and_then(unguarded)
    }

    fn locate_by_message_id(
        &mut self,
        mailbox_path: &str,
        message_id: &str,
    ) -> Result<Vec<i64>, ProviderError> {
        let proto = session_err_to_provider_err;
        self.session.select(mailbox_path).map_err(proto)?;
        // SEARCH HEADER is a SUBSTRING match: "<a@x>" also finds "<aa@x>" or a
        // crafted "<a@x>.evil". It only narrows the candidates; each one's real
        // Message-ID header is fetched and compared exactly before it counts.
        let candidates = self
            .session
            .uid_search_by_message_id(message_id)
            .map_err(proto)?;
        self.exact_message_id_uids(&candidates, message_id)
    }

    fn move_message_checked(
        &mut self,
        src_path: &str,
        expected_uid_validity: i64,
        uid: i64,
        _message_id: &str,
        dest_path: &str,
    ) -> Result<Guarded<()>, ProviderError> {
        self.move_message_guarded(src_path, Some(expected_uid_validity), uid, dest_path)
            .map(|g| match g {
                Guarded::Applied(_) => Guarded::Applied(()),
                Guarded::UidValidityChanged { current } => Guarded::UidValidityChanged { current },
            })
    }

    fn move_message_reporting(
        &mut self,
        src_path: &str,
        expected_uid_validity: i64,
        uid: i64,
        _message_id: &str,
        dest_path: &str,
    ) -> Result<Guarded<Option<MovedTo>>, ProviderError> {
        self.move_message_guarded(src_path, Some(expected_uid_validity), uid, dest_path)
    }

    fn append(
        &mut self,
        mailbox_path: &str,
        message_id: &str,
        raw: &[u8],
    ) -> Result<(), ProviderError> {
        let proto = session_err_to_provider_err;

        // Ensure the destination exists (auto-create "Sent" on servers that
        // lack it). CREATE on an existing mailbox errors harmlessly — ignore it.
        let _ = self.session.create(mailbox_path);

        // Idempotency: a retried `sent_copy` op must never duplicate the copy.
        // Our Message-IDs are generated per submission (unique), so a match can
        // only be a prior attempt of this same append (or the server filing the
        // submission itself — either way, the copy is already there).
        if !message_id.is_empty() {
            self.session.select(mailbox_path).map_err(proto)?;
            let present = !self
                .session
                .uid_search_by_message_id(message_id)
                .map_err(proto)?
                .is_empty();
            if present {
                return Ok(());
            }
        }

        // \Seen: the user's own sent mail must not surface as unread new mail.
        self.session
            .append(mailbox_path, &[SessionFlag::Seen], raw)
            .map_err(proto)
    }

    fn append_with_flags_get_uid(
        &mut self,
        mailbox_path: &str,
        flags: &[MessageFlag],
        raw: &[u8],
    ) -> Result<Option<AppendedUid>, ProviderError> {
        // Best-effort create, exactly like `append`'s existing defensive
        // CREATE - a missing Drafts folder on a fresh account must not
        // block the very first save. Failure is ignored: the APPEND below
        // is the real signal of whether the mailbox is usable.
        let _ = self.session.create(mailbox_path);
        let wire_flags: Vec<SessionFlag> =
            flags.iter().copied().map(message_flag_to_session).collect();
        let appended = self
            .session
            .append_get_uid(mailbox_path, &wire_flags, raw)
            .map_err(session_err_to_provider_err)?;
        Ok(appended.map(|a| AppendedUid {
            uid_validity: a.uid_validity as i64,
            uid: a.uid as i64,
        }))
    }

    fn find_uid_by_message_id(
        &mut self,
        mailbox_path: &str,
        message_id: &str,
    ) -> Result<Option<AppendedUid>, ProviderError> {
        if message_id.is_empty() {
            return Ok(None);
        }
        let state = self
            .session
            .select(mailbox_path)
            .map_err(session_err_to_provider_err)?;
        let candidates = self
            .session
            .uid_search_by_message_id(message_id)
            .map_err(session_err_to_provider_err)?;
        // Exact header matches only: the UID found here is recorded as the
        // draft's mirror and later expunged, so a substring decoy must never
        // be returned.
        let uids = self.exact_message_id_uids(&candidates, message_id)?;
        // SEARCH can return more than one hit if a prior cleanup failed to
        // expunge an older copy that happened to share a Message-ID (should
        // not occur - this plan generates a fresh Message-ID per save - but
        // if it ever does, the highest UID is the newest copy).
        // -1 = server didn't report UIDVALIDITY -> treat as unknown, same
        // convention as `MailProvider::select`'s own conversion above.
        let uid_validity = state.uid_validity.map(|v| v as i64).unwrap_or(-1);
        Ok(uids
            .into_iter()
            .max()
            .map(|uid| AppendedUid { uid_validity, uid }))
    }

    fn find_uids_by_message_id(
        &mut self,
        mailbox_path: &str,
        message_id: &str,
    ) -> Result<MessageIdMatches, ProviderError> {
        if message_id.is_empty() {
            return Ok(MessageIdMatches {
                uid_validity: 0,
                uids: Vec::new(),
            });
        }
        let state = self
            .session
            .select(mailbox_path)
            .map_err(session_err_to_provider_err)?;
        let candidates = self
            .session
            .uid_search_by_message_id(message_id)
            .map_err(session_err_to_provider_err)?;
        // Exact header matches only: every UID but the newest is queued for
        // STORE \Deleted + EXPUNGE by the server-draft cleanup.
        let uids = self.exact_message_id_uids(&candidates, message_id)?;
        Ok(MessageIdMatches {
            // -1 = the server reported no UIDVALIDITY (same convention as `select`).
            uid_validity: state.uid_validity.map(|v| v as i64).unwrap_or(-1),
            uids,
        })
    }

    fn delete_uid(&mut self, mailbox_path: &str, uid: i64) -> Result<(), ProviderError> {
        self.delete_uid_guarded(mailbox_path, None, uid)
            .and_then(unguarded)
    }

    fn delete_uid_checked(
        &mut self,
        mailbox_path: &str,
        expected_uid_validity: i64,
        uid: i64,
    ) -> Result<Guarded<()>, ProviderError> {
        self.delete_uid_guarded(mailbox_path, Some(expected_uid_validity), uid)
    }

    fn expunge_mailbox(&mut self, mailbox_path: &str, uids: &str) -> Result<(), ProviderError> {
        self.expunge_mailbox_guarded(mailbox_path, None, uids)
            .and_then(unguarded)
    }

    fn expunge_mailbox_checked(
        &mut self,
        mailbox_path: &str,
        expected_uid_validity: i64,
        uids: &str,
    ) -> Result<Guarded<()>, ProviderError> {
        self.expunge_mailbox_guarded(mailbox_path, Some(expected_uid_validity), uids)
    }

    fn qresync_available(&self) -> bool {
        self.qresync_enabled
    }

    fn last_highest_modseq(&self) -> Option<i64> {
        self.last_highest_modseq
    }

    fn select_qresync(
        &mut self,
        mailbox_path: &str,
        uid_validity: i64,
        highest_modseq: i64,
    ) -> Result<QresyncSync, ProviderError> {
        if !self.qresync_enabled {
            return Err(ProviderError::Unsupported(
                "server did not negotiate CONDSTORE/QRESYNC on this connection".into(),
            ));
        }
        let uid_validity_u32 = u32::try_from(uid_validity)
            .map_err(|_| ProviderError::Protocol("uid_validity out of range for QRESYNC".into()))?;
        let modseq_u64 = u64::try_from(highest_modseq).map_err(|_| {
            ProviderError::Protocol("highest_modseq out of range for QRESYNC".into())
        })?;

        let result = self
            .session
            .select_qresync(mailbox_path, uid_validity_u32, modseq_u64)
            .map_err(session_err_to_provider_err)?;
        self.last_highest_modseq = result.mailbox.highest_modseq.map(|v| v as i64);

        Ok(QresyncSync {
            state: MailboxState {
                uid_validity: result.mailbox.uid_validity.map(|v| v as i64).unwrap_or(-1),
                uid_next: result.mailbox.uid_next.map(|v| v as i64).unwrap_or(0),
                exists: result.mailbox.exists as i64,
            },
            highest_modseq: result.mailbox.highest_modseq.map(|v| v as i64),
            changed: result
                .changed
                .into_iter()
                .filter_map(|f| {
                    let uid = f.uid? as i64;
                    Some(UidFlags {
                        uid,
                        is_seen: f.flags.contains(&SessionFlag::Seen),
                        is_flagged: f.flags.contains(&SessionFlag::Flagged),
                        keywords: Some(f.keywords),
                    })
                })
                .collect(),
            vanished: UidRangeSet::from_ranges(
                result
                    .vanished_uid_ranges
                    .into_iter()
                    .map(|r| i64::from(*r.start())..=i64::from(*r.end())),
            ),
        })
    }

    fn idle_supported(&self) -> bool {
        self.cached_capabilities
            .iter()
            .any(|c| c.eq_ignore_ascii_case("IDLE"))
    }

    fn idle_start(&mut self, mailbox_path: &str) -> Result<(), ProviderError> {
        self.session
            .select(mailbox_path)
            .map_err(session_err_to_provider_err)?;
        self.session
            .idle_start()
            .map_err(session_err_to_provider_err)
    }

    fn idle_wait(&mut self, timeout: std::time::Duration) -> Result<IdleWake, ProviderError> {
        match self
            .session
            .idle_wait(timeout)
            .map_err(session_err_to_provider_err)?
        {
            Some(_) => Ok(IdleWake::Activity),
            None => Ok(IdleWake::Timeout),
        }
    }

    fn idle_done(&mut self) -> Result<(), ProviderError> {
        self.session
            .idle_done()
            .map_err(session_err_to_provider_err)
    }
}

/// Parse a dot-separated IMAP section path ("1", "2.1") into numeric part
/// numbers. Never panics: an empty or non-numeric component is a typed
/// `ProviderError`; caller-supplied addressing is validated before it reaches
/// the wire (`Session` additionally rejects a zero component).
fn parse_section_path(section: &str) -> Result<Vec<u32>, ProviderError> {
    if section.is_empty() {
        return Err(ProviderError::Protocol(
            "fetch_attachment_part: empty section".into(),
        ));
    }
    section
        .split('.')
        .map(|p| {
            p.parse::<u32>().map_err(|_| {
                ProviderError::Protocol(format!("invalid attachment section {section:?}"))
            })
        })
        .collect()
}

fn remote_message_from_fetched(f: FetchedMessage, after_uid: Option<i64>) -> Option<RemoteMessage> {
    let uid = f.uid? as i64;
    if let Some(after) = after_uid {
        if uid <= after {
            return None;
        }
    }
    Some(RemoteMessage {
        uid,
        raw: f.body?,
        is_seen: f.flags.contains(&SessionFlag::Seen),
        is_flagged: f.flags.contains(&SessionFlag::Flagged),
    })
}

fn remote_address(a: &SessionAddress) -> RemoteAddress {
    RemoteAddress {
        name: a.name.clone(),
        address: a.address.clone(),
    }
}

fn remote_addresses(addrs: &[SessionAddress]) -> Vec<RemoteAddress> {
    addrs.iter().map(remote_address).collect()
}

/// Role from RFC 6154 SPECIAL-USE `LIST` attributes (`\Trash`, `\Archive`, …).
/// Most reliable when present.
fn role_from_attributes(attrs: &[String]) -> Option<String> {
    for a in attrs {
        let role = match a.as_str() {
            "\\Trash" => "trash",
            "\\Archive" | "\\All" => "archive", // Gmail "All Mail" is the archive target
            "\\Junk" => "junk",
            "\\Sent" => "sent",
            "\\Drafts" => "drafts",
            _ => continue,
        };
        return Some(role.to_string());
    }
    None
}

fn role_with_source(attrs: &[String], path: &str) -> (Option<String>, Option<MailboxRoleSource>) {
    match role_from_attributes(attrs) {
        Some(role) => (Some(role), Some(MailboxRoleSource::SpecialUse)),
        None => match detect_role(path) {
            Some(role) => (Some(role), Some(MailboxRoleSource::Heuristic)),
            None => (None, None),
        },
    }
}

/// Best-effort semantic role from a well-known mailbox path (fallback when the
/// server doesn't advertise SPECIAL-USE).
fn detect_role(path: &str) -> Option<String> {
    let last = path.rsplit(['/', '.']).next().unwrap_or(path);
    let role = match last.to_ascii_lowercase().as_str() {
        "inbox" => "inbox",
        "sent" | "sent items" | "sent mail" => "sent",
        "drafts" => "drafts",
        "trash" | "deleted" | "deleted items" | "deleted messages" => "trash",
        "junk" | "spam" | "bulk mail" => "junk",
        "archive" | "all mail" | "archived" => "archive",
        _ => return None,
    };
    Some(role.to_string())
}

/// Display leaf name for a hierarchical path.
fn leaf_name(path: &str, delimiter: Option<&str>) -> String {
    let d = delimiter.unwrap_or("/");
    path.rsplit(d).next().unwrap_or(path).to_string()
}

/// A single-message COPYUID as the provider-level destination record.
fn moved_to(landed: Option<CopyUid>) -> Option<MovedTo> {
    landed.map(|c| MovedTo {
        uid_validity: i64::from(c.uid_validity),
        uid: i64::from(c.uid),
    })
}

#[cfg(test)]
mod tests {

    // -- STARTTLS through the provider --------------------

    /// A scripted cleartext server that records every command and answers
    /// STARTTLS with `starttls_reply`; LOGIN, CAPABILITY and the rest get OK.
    fn starttls_server(
        starttls_reply: &'static str,
    ) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{BufRead, BufReader};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            let mut w = stream.try_clone().expect("clone");
            let mut r = BufReader::new(stream);
            let _ = w.write_all(b"* OK [CAPABILITY IMAP4rev1 STARTTLS LOGINDISABLED] ready\r\n");
            let mut line = String::new();
            loop {
                line.clear();
                match r.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                let mut parts = line.trim_end().splitn(3, ' ');
                let tag = parts.next().unwrap_or("").to_string();
                let cmd = parts.next().unwrap_or("").to_ascii_uppercase();
                log.lock().unwrap().push(cmd.clone());
                let reply = match cmd.as_str() {
                    "STARTTLS" => starttls_reply.replace("TAG", &tag),
                    "CAPABILITY" => format!("* CAPABILITY IMAP4rev1 IDLE\r\n{tag} OK done\r\n"),
                    "LOGOUT" => format!("* BYE bye\r\n{tag} OK bye\r\n"),
                    _ => format!("{tag} OK done\r\n"),
                };
                let _ = w.write_all(reply.as_bytes());
                // After a refused STARTTLS the loop keeps listening, so a
                // client that wrongly carried on would be caught sending LOGIN.
                let _ = w.flush();
            }
        });
        (port, seen)
    }

    fn starttls_cfg(port: u16) -> ImapConfig {
        ImapConfig {
            host: "127.0.0.1".into(),
            port,
            username: "alice".into(),
            password: "secret".into(),
            security: TransportSecurity::StartTls,
            allow_invalid_certs: false,
        }
    }

    /// Stands in for the TLS handshake so the command order can be read.
    fn passthrough(_: &ImapConfig, tcp: TcpStream) -> Result<Box<dyn ReadWrite>, ProviderError> {
        Ok(Box::new(tcp))
    }

    fn connect_scripted(port: u16) -> Result<ImapProvider, ProviderError> {
        let tcp = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        ImapProvider::connect_on(
            &starttls_cfg(port),
            tcp,
            std::time::Duration::from_secs(5),
            passthrough,
        )
    }

    #[test]
    fn starttls_mode_upgrades_before_login_and_asks_capability_again() {
        let (port, seen) = starttls_server("TAG OK Begin TLS\r\n");
        let provider = connect_scripted(port).expect("STARTTLS then LOGIN");
        drop(provider);
        let cmds = seen.lock().unwrap().clone();
        assert_eq!(&cmds[..3], &["STARTTLS", "LOGIN", "CAPABILITY"], "{cmds:?}");
    }

    #[test]
    fn a_refused_starttls_is_a_tls_error_and_never_sends_the_password() {
        for reply in ["TAG NO not now\r\n", "TAG BAD what\r\n"] {
            let (port, seen) = starttls_server(reply);
            match connect_scripted(port) {
                Err(ProviderError::Tls(_)) => {}
                Err(other) => panic!("expected Tls, got {other:?}"),
                Ok(_) => panic!("a refused STARTTLS must not connect"),
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            let cmds = seen.lock().unwrap().clone();
            assert!(!cmds.iter().any(|c| c == "LOGIN"), "{cmds:?}");
        }
    }

    #[test]
    fn an_injected_response_after_the_starttls_ok_is_a_tls_error() {
        let (port, seen) =
            starttls_server("TAG OK Begin TLS\r\n* OK [CAPABILITY IMAP4rev1] injected\r\n");
        assert!(matches!(connect_scripted(port), Err(ProviderError::Tls(_))));
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!seen.lock().unwrap().iter().any(|c| c == "LOGIN"));
    }

    use super::*;

    #[test]
    fn max_skips_per_request_uses_the_smaller_of_5pct_and_20() {
        // A DESCRIPTOR_CHUNK_SIZE-sized (1000) chunk: 5% would be 50, but 20
        // is smaller and wins.
        assert_eq!(max_skips_per_request(1000), 20);
        // A 100-UID chunk: 5% is exactly 5, which is smaller than 20.
        assert_eq!(max_skips_per_request(100), 5);
        // Small chunks: 5% rounds up, never to zero for a non-empty chunk.
        assert_eq!(max_skips_per_request(4), 1);
        assert_eq!(max_skips_per_request(20), 1);
        assert_eq!(max_skips_per_request(21), 2);
        assert_eq!(max_skips_per_request(1), 1);
        assert_eq!(max_skips_per_request(0), 0);
    }

    #[test]
    fn uid_runs_collapse_contiguous_uids_and_drop_invalid_ones() {
        // Review of PR #180, nit 5: the provider builds a compact set itself
        // instead of a comma-joined list of every UID (and no longer relies
        // on imap-codec's default-on normalisation to shorten it on the wire).
        let runs = UidRuns::from_uids(&[9, 1, 2, 3, 4, 5, 10, 20, 3, 0, -4, 5_000_000_000]);
        assert_eq!(runs.as_set_string(), "1:5,9:10,20");
        assert_eq!(runs.len(), 8);
        assert_eq!(runs.last(), Some(20));
        assert!(runs.contains(4) && runs.contains(10) && !runs.contains(6));

        let range = UidRuns::from_range(-3, 4);
        assert_eq!(range.as_set_string(), "1:4");
        assert!(UidRuns::from_range(5, 3).is_empty());
        assert_eq!(UidRuns::from_range(7, 7).as_set_string(), "7");
    }

    #[test]
    fn uid_runs_split_into_fixed_size_batches_in_order() {
        let batches: Vec<String> = UidRuns::from_range(1, 70)
            .batches(32)
            .iter()
            .map(UidRuns::as_set_string)
            .collect();
        assert_eq!(batches, vec!["1:32", "33:64", "65:70"]);

        let sparse = UidRuns::from_uids(&[1, 2, 3, 10, 11, 50]);
        let batches: Vec<String> = sparse
            .batches(4)
            .iter()
            .map(UidRuns::as_set_string)
            .collect();
        assert_eq!(batches, vec!["1:3,10", "11,50"]);
        assert_eq!(sparse.uids().collect::<Vec<_>>(), vec![1, 2, 3, 10, 11, 50]);
    }

    #[test]
    fn leaf_name_uses_provider_delimiter() {
        assert_eq!(leaf_name("Projects.2026.Client", Some(".")), "Client");
        assert_eq!(leaf_name("Projects/2026/Client", Some("/")), "Client");
    }

    #[test]
    fn special_use_role_provenance_outranks_name_heuristics() {
        let attrs = ["\\Sent".to_string()];
        assert_eq!(
            role_with_source(&attrs, "Ordinary Folder"),
            (
                Some("sent".to_string()),
                Some(MailboxRoleSource::SpecialUse)
            )
        );
        assert_eq!(
            role_with_source(&[], "Sent"),
            (Some("sent".to_string()), Some(MailboxRoleSource::Heuristic))
        );
    }

    #[test]
    fn section_paths_are_validated() {
        assert_eq!(parse_section_path("2.1").unwrap(), vec![2, 1]);
        assert!(parse_section_path("").is_err());
        assert!(parse_section_path("1.x").is_err());
        assert!(parse_section_path("1..2").is_err());
    }

    /// #3040: one clock. A descriptor's `sent_at` is the ENVELOPE Date header
    /// (what body ingest parses), INTERNALDATE only when there is none.
    #[test]
    fn descriptor_sent_at_prefers_the_envelope_date_over_internaldate() {
        let mk = |date_epoch: Option<i64>| FetchedMessage {
            uid: Some(3),
            flags: Vec::new(),
            keywords: Vec::new(),
            size_bytes: None,
            envelope: Some(crate::session::SessionEnvelope {
                subject: None,
                from: Vec::new(),
                to: Vec::new(),
                cc: Vec::new(),
                bcc: Vec::new(),
                message_id: None,
                in_reply_to: Vec::new(),
                date_epoch,
            }),
            internal_date_epoch: Some(1_000),
            has_attachments: None,
            body: None,
        };
        assert_eq!(
            descriptor_from_fetched(mk(Some(5_000))).unwrap().sent_at,
            Some(5_000)
        );
        assert_eq!(
            descriptor_from_fetched(mk(None)).unwrap().sent_at,
            Some(1_000)
        );
    }

    #[test]
    fn full_body_response_without_body_is_skipped() {
        let mut f = FetchedMessage {
            uid: Some(7),
            flags: vec![SessionFlag::Seen],
            keywords: Vec::new(),
            size_bytes: None,
            envelope: None,
            internal_date_epoch: None,
            has_attachments: None,
            body: None,
        };
        assert!(remote_message_from_fetched(f.clone(), None).is_none());
        f.body = Some(b"body".to_vec());
        let message = remote_message_from_fetched(f.clone(), None).unwrap();
        assert_eq!(message.uid, 7);
        assert_eq!(message.raw, b"body");
        assert!(message.is_seen);
        assert!(!message.is_flagged);
        assert!(remote_message_from_fetched(f, Some(7)).is_none());
    }

    #[test]
    fn provider_error_mapping_keeps_the_tagged_text() {
        let e = SessionError::No {
            command: "LOGIN",
            text: "bad".into(),
        };
        assert!(matches!(
            session_err_to_provider_err(e),
            ProviderError::Protocol(m) if m == "LOGIN: bad"
        ));
    }
}
