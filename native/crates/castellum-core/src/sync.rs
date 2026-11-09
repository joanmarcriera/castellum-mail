// Castellum - read-only IMAP sync engine.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Orchestrates provider + database: list mailboxes, resume from the UID cursor
// (full refetch when UIDVALIDITY changes), fetch new messages, parse, and upsert.
// Read-only: nothing here mutates remote state. Blocking (no Tokio yet — there is
// no concurrent async workload; the DB's single writer preserves ordering).

use crate::imap_connect::{connect_guarded_imap, connect_guarded_with};
use crate::pass_gate;
use crate::redact::{self, phase};
use crate::sync_progress;
use crate::sync_timing;
use crate::CoreError;
use castellum_db::tags::normalise_keyword;
use castellum_db::{Database, DbError};
use castellum_mail::parse_message;
use castellum_models::{
    Account, Address, Mailbox, MessageDetail, NewMessage, OperationRecord, SyncCursor,
};
use castellum_provider_imap::ImapProvider;
use castellum_providers::{
    DescriptorChunkResult, DescriptorFetchIncomplete, DescriptorFetchStop, ImapConfig,
    MailProvider, MailboxRoleSource, MailboxState, PanicGuardedProvider, ProviderError,
    QresyncSync, RemoteAddress, RemoteMailbox, RemoteMessage, RemoteMessageDescriptor, UidFlags,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

const BODY_HYDRATION_BATCH_LIMIT: i64 = 25;

/// Messages per `Database::upsert_messages_batch` transaction during
/// descriptor import. Long enough to amortise the
/// transaction's own commit cost, short enough that a concurrent UI read
/// queued behind it stays imperceptible: measured p95 ~6-8ms per 200-message
/// batch (mock-provider `sync_perf_gate`, 100k-message corpus) -- see
/// `Database::upsert_messages_batch`'s doc comment for why that number,
/// not the pre-#2503 per-100-message baseline, is the right one to cite.
/// `pub` so `examples/sync_perf_gate.rs` and `examples/sync_bench.rs` can
/// derive true per-message stats from the now-per-batch
/// `db.upsert_descriptor` timing samples instead of guessing the batch size.
pub const DESCRIPTOR_UPSERT_BATCH_SIZE: usize = 200;

/// UIDs per `MailProvider::fetch_descriptor_chunk` call during a mailbox's
/// first/from-scratch descriptor import. Chosen so a chunk's
/// own wire response and DB commit stay small and checkpointable — a real
/// Gmail INBOX first sync buffered a single unbounded `UID FETCH n:*` for
/// ~66s/~64MiB before this landed (see PROJECT_STATE.md / the S29 pilot
/// notes); ~1000 UIDs keeps each chunk in the low-single-digit-MiB range
/// while still amortising the round trip over the old per-message
/// `UID FETCH n:n+1` extreme. `pub` so `examples/sync_bench.rs` and
/// `examples/sync_perf_gate.rs` can size their own scripted checks against it.
pub const DESCRIPTOR_CHUNK_SIZE: i64 = 1000;

/// Cap on how many previously-skipped UIDs the retry pass re-fetches per
/// SYNC PASS, across every mailbox (review of PR #180, round 2, finding 3:
/// the first cut retried up to 200 per mailbox, every pass, before the
/// import and on its reconnect budget). Only rows whose backoff has expired
/// are candidates, and the provider spends a separate 2-reconnect budget on
/// them, so a large or permanently bad retry set can never starve an
/// import; a backlog simply drains over several passes.
const RETRY_UIDS_PER_PASS: i64 = 20;

/// Unix epoch seconds for the decode-skip retry backoff, clock-failure-safe
/// (0 rather than a panic, which only makes every row look due).
fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Accumulates one mailbox's chunked descriptor import across every
/// successfully committed chunk — the summary
/// chunked import (`sync_with_provider_scoped`) turns into the same
/// `(fetched, descriptors_fetched, ...)` shape the legacy unbounded path
/// produces, so both paths feed one `MailboxSyncReport`.
#[derive(Default)]
struct ChunkedDescriptorFetch {
    fetched: usize,
    uid_min: Option<i64>,
    uid_max: Option<i64>,
    skipped: usize,
}

/// Split descriptors into the `NewMessage` rows `Database::upsert_messages_batch`
/// takes and, index-aligned with them, each row's server keyword set (S42) --
/// `None` when the provider reported none or an empty set, so a brand-new
/// message without keywords costs no extra write and first-sync cost stays
/// flat. Every descriptor-persisting path (the chunked import, the unbounded
/// fallback and the decode-skip retry) goes through this, so none of them can
/// drop keywords the provider did report.
fn descriptor_rows(
    account_id: &str,
    mailbox_id: &str,
    descriptors: Vec<RemoteMessageDescriptor>,
) -> (Vec<NewMessage>, Vec<Option<Vec<String>>>) {
    descriptors
        .into_iter()
        .map(|mut d| {
            let keywords = d.keywords.take().filter(|kw| !kw.is_empty());
            (
                new_message_from_descriptor(account_id, mailbox_id, d),
                keywords,
            )
        })
        .unzip()
}

/// Write the non-empty server keyword sets [`descriptor_rows`] split off, for
/// rows that `upsert_messages_batch` has just persisted (the keyword rows
/// reference `messages(id)`, so never before it).
fn write_descriptor_keywords(
    db: &Database,
    rows: &[NewMessage],
    keywords: &[Option<Vec<String>>],
) -> Result<(), DbError> {
    for (nm, kw) in rows.iter().zip(keywords) {
        if let Some(kw) = kw {
            db.replace_server_keywords(&nm.id, kw)?;
        }
    }
    Ok(())
}

/// Ids of the messages a sync pass newly stored, kept twice (MM-3 review):
/// `all` feeds the mute-on-arrival pass exactly as before, while `for_rules`
/// holds only GENUINE ARRIVALS — mail stored while `rules_eligible` was set,
/// which the caller does only for an Inbox-role folder whose UID space is
/// unchanged since a completed earlier import. A first import (the whole
/// history), a resumed interrupted first import, a UIDVALIDITY reset's full
/// re-import, any other folder (where our own moves land) and decode-skip
/// retries (old mail) are never handed to the rules: running them there
/// re-applied rules to years of mail and undid the user's moves.
#[derive(Default)]
struct NewArrivals {
    all: Vec<String>,
    for_rules: Vec<String>,
    rules_eligible: bool,
}

impl NewArrivals {
    fn extend<I: IntoIterator<Item = String>>(&mut self, ids: I) {
        for id in ids {
            if self.rules_eligible {
                self.for_rules.push(id.clone());
            }
            self.all.push(id);
        }
    }
}

/// Persist one descriptor chunk
/// through #178's batch path, record any isolated decode skips (never
/// best-effort — see below), and durably checkpoint. `requested_high` is the
/// highest UID the request asked for (the dense range's high end, or the
/// sparse group's highest UID); the checkpoint goes there when the provider
/// covered the whole request, or to `incomplete.covered_through` when it
/// stopped early (review of PR #180, round 2: progress made before a budget
/// stop is committed, not thrown away) — either way "everything up to and
/// including this UID has been imported, recorded as skipped, or does not
/// exist". Nothing covered, nothing to checkpoint. Returns the stop, if
/// any, for the caller to fail the mailbox with. Shared by both chunking
/// strategies so persistence/checkpoint/skip-recording behaviour can never
/// drift between them.
#[allow(clippy::too_many_arguments)]
fn apply_descriptor_chunk(
    db: &Database,
    account_id: &str,
    mailbox_id: &str,
    uid_validity: i64,
    chunk: DescriptorChunkResult,
    requested_high: i64,
    acc: &mut ChunkedDescriptorFetch,
    new_message_ids: &mut NewArrivals,
) -> Result<Option<DescriptorFetchIncomplete>, CoreError> {
    let incomplete = chunk.incomplete;
    let checkpoint_to = match incomplete {
        None => Some(requested_high),
        Some(stop) => stop.covered_through,
    };
    acc.uid_min = [acc.uid_min, chunk.descriptors.iter().map(|d| d.uid).min()]
        .into_iter()
        .flatten()
        .min();
    acc.uid_max = [acc.uid_max, chunk.descriptors.iter().map(|d| d.uid).max()]
        .into_iter()
        .flatten()
        .max();
    acc.fetched += chunk.descriptors.len();
    let (new_messages, keywords) = descriptor_rows(account_id, mailbox_id, chunk.descriptors);
    new_message_ids.extend(new_messages.iter().map(|nm| nm.id.clone()));
    // Same #178 batch path as the unbounded descriptor fetch, applied per
    // network chunk instead of once for the whole mailbox — bounds memory
    // (a chunk's descriptors, not the whole first sync) and advances
    // progress every chunk, not once at the end.
    for (sub, sub_keywords) in new_messages
        .chunks(DESCRIPTOR_UPSERT_BATCH_SIZE)
        .zip(keywords.chunks(DESCRIPTOR_UPSERT_BATCH_SIZE))
    {
        sync_timing::time("db.upsert_descriptor", || -> Result<(), DbError> {
            db.upsert_messages_batch(sub)?;
            write_descriptor_keywords(db, sub, sub_keywords)
        })
        .map_err(db_err)?;
        sync_progress::advance(account_id, sub.len() as i64);
    }
    // Never `let _ =` here (Opus review of PR #180): "only a UID that has
    // actually been recorded as a skip may be passed by the checkpoint" is
    // an invariant, not a best effort — a swallowed DB error would advance
    // the checkpoint below past a UID nothing durable remembers, losing it
    // exactly like the un-tracked-skip bug the retry set exists to fix.
    let now = now_epoch();
    for skip in &chunk.skipped {
        db.record_message_decode_skip(
            account_id,
            mailbox_id,
            skip.uid,
            uid_validity,
            skip.reason.as_str(),
            now,
        )
        .map_err(db_err)?;
    }
    acc.skipped += chunk.skipped.len();
    // Durable checkpoint: this chunk/group (or its covered prefix) and
    // everything below it is now committed, so an interrupt after this
    // point resumes past it, never from UID 1. Never touches `uid_next`/
    // `highest_modseq` — see migration 0040's comment on why those stay
    // `persist_cursor`'s job, once the WHOLE mailbox finishes.
    if let Some(checkpoint_to) = checkpoint_to {
        db.set_descriptor_checkpoint(account_id, mailbox_id, uid_validity, checkpoint_to)
            .map_err(db_err)?;
    }
    Ok(incomplete)
}

/// The error a mailbox's report and diagnostic row carry when a descriptor
/// fetch stopped early: the stop's fixed text, never server content. Its
/// redacted kind (see `redact::redact_provider_error`) is `network` for an
/// interruption — in practice a dropped connection or a server BYE — and
/// `protocol` for the decode-failure budgets.
fn descriptor_stop_error(stop: DescriptorFetchIncomplete) -> ProviderError {
    let text = stop.reason.as_str().to_string();
    match stop.reason {
        DescriptorFetchStop::Interrupted => ProviderError::Network(text),
        DescriptorFetchStop::ReconnectBudgetExhausted
        | DescriptorFetchStop::SkipBudgetExceeded
        | DescriptorFetchStop::SessionAbandoned => ProviderError::Protocol(text),
    }
}

/// The decode-skip retry pass for one mailbox (review of PR #180, round 2,
/// finding 3), run AFTER its import has been checkpointed: re-fetch the rows
/// whose backoff has expired, at most `*uids_left` of them (the pass-wide
/// `RETRY_UIDS_PER_PASS` allowance, decremented here by the UIDs actually
/// attempted), on the provider's separate retry budget. A UID that now
/// decodes is persisted and leaves the set; one the server no longer has
/// leaves it too; one that still fails backs off (4h, 24h, then 7d). A
/// provider without retry support, or any retry error, leaves every row as
/// it is for a later pass — retries are never on a mailbox's critical path.
#[allow(clippy::too_many_arguments)]
fn retry_decode_skips(
    db: &Database,
    account_id: &str,
    mailbox_id: &str,
    path: &str,
    uid_validity: i64,
    provider: &mut dyn MailProvider,
    uids_left: &mut i64,
    new_message_ids: &mut Vec<String>,
) -> Result<(), CoreError> {
    if *uids_left <= 0 {
        return Ok(());
    }
    let now = now_epoch();
    // A failure to even list the set is not this pass's problem either
    // (e.g. a dev profile that applied an earlier draft of migration 0040
    // without the backoff columns): no retries, the mailbox still syncs.
    let Ok(due) =
        db.list_due_message_decode_skips(account_id, mailbox_id, uid_validity, now, *uids_left)
    else {
        return Ok(());
    };
    if due.is_empty() {
        return Ok(());
    }
    let result = match sync_timing::time("imap.retry_descriptor_skips", || {
        provider.retry_descriptor_skips(path, &due)
    }) {
        Ok(result) => result,
        Err(_) => return Ok(()),
    };
    // Every due UID up to `covered_through` was attempted; above it, none.
    let attempted: Vec<i64> = match result.incomplete {
        None => due,
        Some(stop) => due
            .into_iter()
            .filter(|uid| stop.covered_through.is_some_and(|c| *uid <= c))
            .collect(),
    };
    *uids_left -= attempted.len() as i64;
    let still_bad: HashSet<i64> = result.skipped.iter().map(|s| s.uid).collect();
    let (recovered, recovered_keywords) = descriptor_rows(
        account_id,
        mailbox_id,
        result
            .descriptors
            .into_iter()
            .filter(|d| attempted.contains(&d.uid))
            .collect(),
    );
    new_message_ids.extend(recovered.iter().map(|nm| nm.id.clone()));
    if !recovered.is_empty() {
        db.upsert_messages_batch(&recovered).map_err(db_err)?;
        write_descriptor_keywords(db, &recovered, &recovered_keywords).map_err(db_err)?;
        sync_progress::advance(account_id, recovered.len() as i64);
    }
    for uid in attempted {
        if still_bad.contains(&uid) {
            db.record_message_decode_retry_failure(account_id, mailbox_id, uid_validity, uid, now)
                .map_err(db_err)?;
        } else {
            // Decoded (persisted above) or no longer on the server: either
            // way nothing is left to retry.
            db.delete_message_decode_skip(account_id, mailbox_id, uid_validity, uid)
                .map_err(db_err)?;
        }
    }
    Ok(())
}

/// Most UIDs re-fetched per mailbox per pass (the rest wait for the next one).
const REFETCH_PER_PASS: i64 = 200;

/// Re-fetch messages whose archive/delete/Empty Trash the user CANCELLED (bug
/// hunt #69). The queued op deleted the local row, the server still holds the
/// message, and incremental sync never re-imports below UIDNEXT, so the
/// message stayed hidden. The UIDs were recorded at cancel time in
/// `pending_refetch`; here they come back through the normal descriptor
/// upsert, as OLD mail (joined to `all` only, never rules-eligible). Rows
/// under another UIDVALIDITY are dropped by the query. UIDs the server no
/// longer has simply leave the set. Never fails the mailbox: any error leaves
/// the rows for a later pass.
fn refetch_cancelled_removals(
    db: &Database,
    account_id: &str,
    mailbox_id: &str,
    path: &str,
    uid_validity: i64,
    provider: &mut dyn MailProvider,
    new_message_ids: &mut Vec<String>,
) -> Result<(), CoreError> {
    let Ok(uids) = db.pending_refetch_uids(account_id, path, uid_validity, REFETCH_PER_PASS) else {
        return Ok(());
    };
    if uids.is_empty() {
        return Ok(());
    }
    let result = match sync_timing::time("imap.fetch_descriptor_list", || {
        provider.fetch_descriptor_list(path, &uids)
    }) {
        Ok(result) => result,
        Err(_) => return Ok(()),
    };
    // Every UID up to `covered_through` was attempted; above it, none.
    let attempted: Vec<i64> = match result.incomplete {
        None => uids,
        Some(stop) => uids
            .into_iter()
            .filter(|uid| stop.covered_through.is_some_and(|c| *uid <= c))
            .collect(),
    };
    let undecodable: HashSet<i64> = result.skipped.iter().map(|s| s.uid).collect();
    let (rows, keywords) = descriptor_rows(
        account_id,
        mailbox_id,
        result
            .descriptors
            .into_iter()
            .filter(|d| attempted.contains(&d.uid))
            .collect(),
    );
    new_message_ids.extend(rows.iter().map(|nm| nm.id.clone()));
    if !rows.is_empty() {
        db.upsert_messages_batch(&rows).map_err(db_err)?;
        write_descriptor_keywords(db, &rows, &keywords).map_err(db_err)?;
        sync_progress::advance(account_id, rows.len() as i64);
    }
    // An undecodable one stays (the decode-skip retry set owns those once the
    // import path sees them; here it simply waits for a later pass).
    let done: Vec<i64> = attempted
        .into_iter()
        .filter(|uid| !undecodable.contains(uid))
        .collect();
    db.clear_pending_refetch(account_id, path, uid_validity, &done)
        .map_err(db_err)?;
    Ok(())
}

/// Fail the mailbox mid-chunk-loop: a real failure (network, auth, a
/// re-SELECT refused after isolation's internal reconnect), or a provider
/// that stopped early on its reconnect/skip budget (`descriptor_stop_error`)
/// — never a lone decode failure, which `fetch_descriptor_chunk`/
/// `fetch_descriptor_list` must already have isolated into a chunk's
/// `skipped` list. Whatever was committed before this stays committed (for
/// a budget stop, including the covered part of this very chunk): the next
/// pass resumes from the checkpoint already written, exactly like any other
/// mid-mailbox interruption. The caller decides `break 'mailbox` vs
/// `continue 'mailbox` afterward via `provider.session_abandoned()` — kept
/// out of this helper so it doesn't need a second mutable borrow of
/// `provider` alongside the caller's own.
#[allow(clippy::too_many_arguments)]
fn fail_mailbox_mid_chunk_loop(
    db: &Database,
    account_id: &str,
    mailbox_id: &str,
    path: &str,
    cancelled_ops: usize,
    acc: &ChunkedDescriptorFetch,
    e: &ProviderError,
    reports: &mut Vec<MailboxSyncReport>,
) -> Result<(), CoreError> {
    let (kind, summary) = redact::redact_provider_error(e);
    let _ = db.record_mailbox_sync_failure(
        account_id,
        mailbox_id,
        kind.as_str(),
        summary,
        phase::FETCH,
    );
    let total = db.count_messages(account_id, mailbox_id).map_err(db_err)?;
    reports.push(MailboxSyncReport {
        path: path.to_string(),
        fetched: acc.fetched,
        descriptors_fetched: acc.fetched,
        bodies_fetched: 0,
        bytes_fetched: 0,
        fetched_uid_min: acc.uid_min,
        fetched_uid_max: acc.uid_max,
        total,
        cancelled_ops,
        flags_reconciled: 0,
        vanished: 0,
        descriptors_skipped: acc.skipped,
        error: Some(e.to_string()),
        skipped: None,
    });
    Ok(())
}

/// Enqueue a background-priority `embed_message` job (S52) for a message
/// whose full body was just persisted, so the local embedding index catches
/// up incrementally without ever slowing down sync/hydration itself: this is
/// a single `INSERT` into `ai_jobs` (see `castellum_db::Database::
/// enqueue_ai_job`) — nothing here calls the AI backend, so the caller
/// returns exactly as fast as it would without this line. The actual
/// embedding computation runs later, out of process, via the existing AI
/// executor (`castellum_core::ai::run::run_ai_jobs_with_backend`), which
/// reads the body fresh from `messages`/`message_bodies` at execution time —
/// only the ref (`message_id`) is ever written here.
///
/// Best-effort: enqueuing can only fail if the payload were malformed, which
/// it structurally cannot be, but a failure here must never fail the sync or
/// hydration pass that just landed the body — swallowed like the diagnostics
/// calls elsewhere in this file.
fn enqueue_embed_job(db: &Database, account_id: &str, message_id: &str) {
    let payload = serde_json::json!({ "message_id": message_id }).to_string();
    let _ = db.enqueue_ai_job(
        Some(account_id),
        "embed_message",
        &payload,
        castellum_db::AI_PRIORITY_BACKGROUND,
    );
}

/// Total bodies one background hydration pass may fetch across all mailboxes.
/// Keeps a pass short (seconds) so repeated passes drain a mature mailbox
/// gradually without starving body-on-open or interactive reads.
const HYDRATION_PASS_TOTAL_LIMIT: i64 = 100;

/// Default pre-fetch byte budget for one background hydration pass when the
/// caller does not provide `hydration_byte_budget`. Matches the Swift
/// per-sync ceiling so even a caller that never threads a budget through
/// cannot fetch more than one ceiling's worth per pass.
pub const HYDRATION_PASS_DEFAULT_BYTE_BUDGET: i64 = 64 << 20;

/// Conservative predicted size for backlog rows with no stored RFC822.SIZE.
/// Descriptor-first sync always stores the size, so this only covers legacy
/// or partially imported rows; the actual fetched bytes are counted against
/// the running budget afterwards regardless.
const HYDRATION_UNKNOWN_SIZE_ESTIMATE: i64 = 512 * 1024;

/// Sync request (deserialized from the FFI JSON).
///
/// `Debug` is implemented manually to redact `password` — a stray `{:?}` of a
/// config (e.g. in an error message) must never leak a credential.
#[derive(Deserialize)]
pub struct ImapSyncConfig {
    pub account_id: String,
    pub email: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    /// Implicit TLS (IMAPS). Defaults to true.
    #[serde(default = "default_true")]
    pub use_tls: bool,
    /// IMAP security mode, "tls" | "starttls" | "none". Absent
    /// = the legacy `use_tls` decides; see `TransportSecurity::resolve`.
    #[serde(default)]
    pub imap_security: Option<String>,
    /// Accept invalid certs — only for explicit local test profiles.
    #[serde(default)]
    pub allow_invalid_certs: bool,
    /// Account display name (S70), persisted to `accounts.display_name`.
    /// Blank/whitespace-only counts as unset.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Restrict sync to these mailbox paths (case-insensitive). None = all.
    #[serde(default)]
    pub mailboxes: Option<Vec<String>>,
    /// Pre-fetch byte budget for one background hydration pass. Candidates
    /// are selected against this budget using stored RFC822.SIZE metadata
    /// BEFORE any body is fetched; bodies larger than the budget stay in the
    /// durable queue for explicit body-on-open hydration. `None` = the
    /// default per-pass budget.
    #[serde(default)]
    pub hydration_byte_budget: Option<i64>,
    /// Initial-history window (S62): `YYYY-MM-DD`. When set, a mailbox's
    /// FIRST-EVER sync (no cursor yet; never a UIDVALIDITY reset, which
    /// re-imports in full so already-held mail isn't lost) imports only
    /// messages whose INTERNALDATE is on/after this date; later syncs resume
    /// from the cursor as usual, so older mail is never back-filled. `None` =
    /// all mail (pre-S62 behaviour). Validated before any network I/O.
    #[serde(default)]
    pub initial_sync_since: Option<String>,
}

/// Convert a validated `YYYY-MM-DD` into an RFC 3501 `date`
/// (`23-Jun-2026`) for `UID SEARCH SINCE`. Strict: anything else is a
/// config error, so free text can never reach the IMAP command line.
pub(crate) fn imap_since_date(ymd: &str) -> Result<String, CoreError> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let bad = || {
        CoreError::Sync(format!(
            "bad initial_sync_since {ymd:?}: expected YYYY-MM-DD"
        ))
    };
    let parts: Vec<&str> = ymd.split('-').collect();
    if parts.len() != 3
        || parts[0].len() != 4
        || parts[1].len() != 2
        || parts[2].len() != 2
        || !parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(bad());
    }
    let year: u32 = parts[0].parse().map_err(|_| bad())?;
    let month: usize = parts[1].parse().map_err(|_| bad())?;
    let day: u32 = parts[2].parse().map_err(|_| bad())?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || year < 1970 {
        return Err(bad());
    }
    Ok(format!("{day}-{}-{year}", MONTHS[month - 1]))
}

impl std::fmt::Debug for ImapSyncConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImapSyncConfig")
            .field("account_id", &self.account_id)
            .field("email", &self.email)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("use_tls", &self.use_tls)
            .field("imap_security", &self.imap_security)
            .field("allow_invalid_certs", &self.allow_invalid_certs)
            .field("mailboxes", &self.mailboxes)
            .field("hydration_byte_budget", &self.hydration_byte_budget)
            .field("initial_sync_since", &self.initial_sync_since)
            .finish()
    }
}

fn default_true() -> bool {
    true
}

/// Result of a sync run.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct SyncReport {
    pub account_id: String,
    pub mailboxes: Vec<MailboxSyncReport>,
    /// Paths of folders this pass removed locally because the server no
    /// longer lists them (deleted or renamed elsewhere;).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_mailboxes: Vec<String>,
    /// Queued operations cancelled because their folder was removed.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub removed_mailbox_cancelled_ops: usize,
    /// Cached attachment content hashes those removals orphaned. The sync
    /// engine has no profile directory, so whoever runs the pass unlinks
    /// the files with `attachments::unlink_orphaned_files`:
    /// `Profile::sync_imap_json` and `resync_on_live_connection`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub orphaned_attachment_hashes: Vec<String>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// Per-mailbox sync outcome.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct MailboxSyncReport {
    pub path: String,
    /// New messages imported from the provider this run.
    pub fetched: usize,
    /// New messages imported through descriptor-only sync this run.
    #[serde(default)]
    pub descriptors_fetched: usize,
    /// Full message bodies fetched this run, either through the legacy body path
    /// or bounded descriptor-row hydration.
    #[serde(default)]
    pub bodies_fetched: usize,
    /// Raw message bytes fetched this run. This is zero for descriptor-only,
    /// skipped, empty, and error paths.
    #[serde(default)]
    pub bytes_fetched: i64,
    /// Lowest provider UID imported this run. On staged sync this is the
    /// descriptor range; on the legacy path it is the full-body range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_uid_min: Option<i64>,
    /// Highest provider UID imported this run. On staged sync this is the
    /// descriptor range; on the legacy path it is the full-body range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_uid_max: Option<i64>,
    /// Total messages stored for the mailbox after the run.
    pub total: i64,
    /// Queued operations cancelled because a UIDVALIDITY reset purged this
    /// mailbox (their UIDs became stale). 0 on a normal incremental sync.
    pub cancelled_ops: usize,
    /// Existing messages whose local flags were updated to match the server
    /// (RFC 4549 flag reconciliation). 0 on first sync / when nothing changed.
    #[serde(default)]
    pub flags_reconciled: usize,
    /// Local messages purged because they vanished from the server
    /// (expunged/moved). 0 when nothing was removed.
    #[serde(default)]
    pub vanished: usize,
    /// Messages a chunked descriptor fetch could not decode and skipped
    ///, never counted in `fetched`/`descriptors_fetched`.
    /// 0 on the legacy unbounded path (which fails the whole fetch instead —
    /// see `error`) and whenever nothing was skipped. Durable detail per skip
    /// lives in the `message_decode_skips` diagnostics table.
    #[serde(default)]
    pub descriptors_skipped: usize,
    /// Why this mailbox was not synced this run, if it wasn't: a container-only
    /// `\Noselect` folder, or a provider error selecting it. `None` on success.
    /// A per-mailbox failure is recorded here and skipped — it never aborts the
    /// whole account sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Non-error reason the mailbox was discovered but not fetched this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

/// Result of one bounded background body-hydration pass.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct HydrationReport {
    pub account_id: String,
    /// Mailboxes visited this pass (skipped-for-budget mailboxes are not
    /// listed; their backlog still counts toward `remaining`).
    pub mailboxes: Vec<MailboxHydrationReport>,
    /// Bodies requested from the provider this pass.
    pub requested: usize,
    /// Bodies fetched and persisted this pass.
    pub hydrated: usize,
    /// Raw message bytes fetched this pass.
    pub bytes_fetched: i64,
    /// Backlog left across ALL mailboxes after the pass. Zero means the
    /// durable missing-body queue is drained; callers stop re-invoking.
    pub remaining: i64,
    pub elapsed_ms: u64,
}

/// Per-mailbox outcome of one hydration pass.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct MailboxHydrationReport {
    pub path: String,
    pub requested: usize,
    pub hydrated: usize,
    pub bytes_fetched: i64,
    pub duration_ms: u64,
    /// Missing bodies left in this mailbox after the pass.
    pub remaining: i64,
    /// Why this mailbox made no (full) progress: select failure, UIDVALIDITY
    /// mismatch, or a body-fetch error. Recorded and skipped — later mailboxes
    /// still hydrate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn provider_err(e: ProviderError) -> CoreError {
    CoreError::Sync(e.to_string())
}

fn db_err<E: std::fmt::Display>(e: E) -> CoreError {
    CoreError::Sync(format!("db: {e}"))
}

/// The pending-op guards reconciliation must honour, built once per sync run
/// from the operation queue so it never clobbers un-replayed local intent.
/// Read and star locks are kept SEPARATE because the two flags are
/// orthogonal — a queued `mark_read` must not freeze a server-side star
/// change, and vice versa. Moved/archived/deleted messages need no guard:
/// the move op's enqueue already deleted their local rows.
struct OpLocks {
    /// Messages with a queued `mark_read` op.
    read_locked: HashSet<String>,
    /// Messages with a queued `flag` op.
    star_locked: HashSet<String>,
    /// Mailboxes with a LEGACY queued `mark_all_read` (no UID snapshot):
    /// locks the read flag of every message in them, but never the star.
    mark_all_mailboxes: HashSet<String>,
    /// Snapshot UID ranges of a queued `mark_all_read`, per mailbox id
    /// (FP27): only those messages' read flag is locked, so mail that arrived
    /// after the press is reconciled from the server as usual.
    mark_all_ranges: HashMap<String, Vec<(i64, i64)>>,
    /// Messages with a queued `keyword` op (S42) — kept SEPARATE from
    /// read/star for the same reason: a queued keyword change must not
    /// freeze a server-side \Seen/\Flagged change, and vice versa.
    keyword_locked: HashSet<String>,
}

impl OpLocks {
    fn from_pending(pending: &[OperationRecord]) -> Self {
        let mut locks = OpLocks {
            read_locked: HashSet::new(),
            star_locked: HashSet::new(),
            mark_all_mailboxes: HashSet::new(),
            mark_all_ranges: HashMap::new(),
            keyword_locked: HashSet::new(),
        };
        locks.absorb(pending);
        locks
    }

    /// Add the guards of `pending` to the ones already held. ADDITIVE on
    /// purpose, never a rebuild: replay is not under the pass gate, so an
    /// op that was pending at pass start may have been replayed by now. Its
    /// lock must still hold for the rest of the pass — the server snapshot
    /// being reconciled can predate the replay, and dropping the lock would
    /// revert the very flag the op just wrote.
    fn absorb(&mut self, pending: &[OperationRecord]) {
        let locks = self;
        for op in pending {
            // A `failed_permanent` op is dead ("needs attention", never
            // replayed on its own): it must not freeze the message's flag or
            // pin it against server-side expunges forever (audit #3451).
            // A manual Retry just re-STOREs the user's value; replay is a
            // no-op if the row has since been purged.
            if op.status == "failed_permanent" {
                continue;
            }
            let payload: serde_json::Value =
                serde_json::from_str(&op.payload_json).unwrap_or(serde_json::Value::Null);
            let field = |key: &str| {
                payload
                    .get(key)
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            };
            match op.kind.as_str() {
                "mark_read" => locks.read_locked.extend(field("message_id")),
                "flag" => locks.star_locked.extend(field("message_id")),
                "mark_all_read" => match (field("mailbox_id"), field("uids")) {
                    (Some(mailbox_id), Some(uids)) => locks
                        .mark_all_ranges
                        .entry(mailbox_id)
                        .or_default()
                        .extend(parse_uid_set(&uids)),
                    (mailbox_id, None) => locks.mark_all_mailboxes.extend(mailbox_id),
                    (None, Some(_)) => {}
                },
                "keyword" => locks.keyword_locked.extend(field("message_id")),
                _ => {}
            }
        }
    }

    /// Pick up operations queued since the pass started (bug hunt
    /// 2026-10-12, #3456). The UI can mark a message read or star it while a
    /// long pass is still working through earlier folders; guards built only
    /// at pass start miss that op, and reconciliation would flip the flag
    /// straight back to the server's older value (or purge a message from
    /// under the op). Called right before each reconciliation step: one
    /// indexed query per mailbox.
    ///
    /// A narrow window remains — an op both queued AND replayed between the
    /// server snapshot and this call is in neither set — and it heals on the
    /// next sync, once the server reports the flag the replay wrote.
    fn refresh(&mut self, db: &Database, account_id: &str) -> Result<(), CoreError> {
        let pending = db.pending_operations(account_id).map_err(db_err)?;
        self.absorb(&pending);
        Ok(())
    }

    /// A message is pinned against purge while any per-message op still
    /// targets it — replay resolves the op first. A queued `keyword` op pins
    /// too: purging the row would cascade away the keyword the op is about
    /// to write (and the op would then target a message that is gone).
    fn pins(&self, msg_id: &str) -> bool {
        self.read_locked.contains(msg_id)
            || self.star_locked.contains(msg_id)
            || self.keyword_locked.contains(msg_id)
    }

    fn read_lock(&self, mailbox_id: &str, msg_id: &str) -> bool {
        if self.read_locked.contains(msg_id) || self.mark_all_mailboxes.contains(mailbox_id) {
            return true;
        }
        let Some(ranges) = self.mark_all_ranges.get(mailbox_id) else {
            return false;
        };
        // Sync builds message ids as "{mailbox_id}:{uid}".
        msg_id
            .strip_prefix(mailbox_id)
            .and_then(|rest| rest.strip_prefix(':'))
            .and_then(|uid| uid.parse::<i64>().ok())
            .is_some_and(|uid| ranges.iter().any(|&(lo, hi)| (lo..=hi).contains(&uid)))
    }

    fn star_lock(&self, msg_id: &str) -> bool {
        self.star_locked.contains(msg_id)
    }

    fn keyword_lock(&self, msg_id: &str) -> bool {
        self.keyword_locked.contains(msg_id)
    }
}

/// Remove the account's stored folders that the server no longer lists — a
/// folder deleted or renamed in another client (bug hunt 2026-10-12, the tracker
/// #3456). Without this the old row keeps its cursor and every cached
/// message in the sidebar and search forever (it is never SELECTed again),
/// and a rename shows every message twice, under the old and the new path.
///
/// This DELETES LOCAL MAIL on the strength of one server answer, so it is
/// deliberately hard to trigger. When in doubt the rows stay:
///
/// - Nothing is removed unless the listing is trustworthy: it must be
///   non-empty, must contain INBOX (a truncated or namespace-limited LIST
///   would otherwise wipe the cache) and the session must not have been
///   abandoned during the pass. A failed LIST never reaches here at all.
/// - A folder must be absent from TWO CONSECUTIVE trustworthy listings. The
///   first one only records it (`mailboxes_absent_from_last_listing`, an
///   `app_meta` row: no migration, survives a relaunch); a folder that
///   reappears, or an untrustworthy listing in between (empty, no INBOX,
///   session abandoned), starts the count again. A LIST that fails
///   outright is different: the pass returns before this function runs, so
///   the recorded set is kept, and absent / LIST error / absent does
///   remove the folder. That is still two successful listings agreeing.
///   Nothing enforces a minimum time between the two, so they can be
///   seconds apart (an IDLE wake, a queued follow-up pass): a glitch has
///   to last across two passes to cost anything, not for any set duration.
/// - Paths are compared exactly as stored (IMAP names are case-sensitive
///   and ids are `account:path`). The stored INBOX is never a candidate,
///   in any casing.
/// - A folder a queued operation still names as its DESTINATION is never a
///   candidate. Archive, delete, send and save-draft create a default-named
///   local row (`Archive`, `Trash`, `Sent`, `Drafts`) when the account has
///   no folder with that role yet; the server only gains it when the op
///   replays, so until then its absence from LIST is expected.
///
/// It runs whether or not the pass had a folder filter: the filter limits
/// what is fetched, not what exists. Operations queued against a removed
/// folder are cancelled with a reason saying the folder is gone — they could
/// only fail on replay, and leaving them would pin the ghost forever.
fn remove_mailboxes_gone_from_server(
    db: &Database,
    account_id: &str,
    remote: &[RemoteMailbox],
    session_abandoned: bool,
) -> Result<castellum_db::RemovedMailboxes, CoreError> {
    let is_inbox = |path: &str| path.eq_ignore_ascii_case("INBOX");
    let trustworthy =
        !remote.is_empty() && remote.iter().any(|m| is_inbox(&m.path)) && !session_abandoned;
    if !trustworthy {
        db.set_mailboxes_absent_from_last_listing(account_id, &[])
            .map_err(db_err)?;
        return Ok(castellum_db::RemovedMailboxes::default());
    }
    let listed: HashSet<&str> = remote.iter().map(|m| m.path.as_str()).collect();
    let awaited: HashSet<String> = db
        .pending_operations(account_id)
        .map_err(db_err)?
        .iter()
        .filter_map(|op| {
            serde_json::from_str::<serde_json::Value>(&op.payload_json)
                .ok()?
                .get("dest_path")?
                .as_str()
                .map(str::to_string)
        })
        .collect();
    let absent: Vec<String> = db
        .list_mailboxes(account_id)
        .map_err(db_err)?
        .into_iter()
        .map(|m| m.path)
        .filter(|path| {
            !listed.contains(path.as_str()) && !is_inbox(path) && !awaited.contains(path)
        })
        .collect();
    let absent_before: HashSet<String> = db
        .mailboxes_absent_from_last_listing(account_id)
        .map_err(db_err)?
        .into_iter()
        .collect();
    let (confirmed, waiting): (Vec<String>, Vec<String>) = absent
        .into_iter()
        .partition(|path| absent_before.contains(path));
    let removed = if confirmed.is_empty() {
        castellum_db::RemovedMailboxes::default()
    } else {
        db.remove_mailboxes_gone_from_server(account_id, &confirmed)
            .map_err(db_err)?
    };
    db.set_mailboxes_absent_from_last_listing(account_id, &waiting)
        .map_err(db_err)?;
    Ok(removed)
}

/// Parse a compact IMAP UID set ("5:6,8") into inclusive ranges. Malformed
/// parts are skipped (they could only lock less, never write anything).
fn parse_uid_set(set: &str) -> Vec<(i64, i64)> {
    set.split(',')
        .filter_map(|part| match part.split_once(':') {
            Some((lo, hi)) => Some((lo.trim().parse().ok()?, hi.trim().parse().ok()?)),
            None => part.trim().parse().ok().map(|u| (u, u)),
        })
        .collect()
}

/// What one reconciliation pass applied, and whether it applied EVERYTHING
/// the server reported. Every reconciliation arm of
/// `sync_with_provider_scoped` (QRESYNC delta, RFC 4549 full scan, emptied
/// mailbox, failed snapshot) records its work ONLY through the methods below,
/// and the cursor's HIGHESTMODSEQ is then chosen by [`modseq_to_persist`]
/// from `applied_all` alone — so no arm can advance the modseq on its own
/// terms (the defect class of S30 review rounds 1 and 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DeltaOutcome {
    flags_reconciled: usize,
    vanished: usize,
    /// False once any server change was held back: a vanished UID left
    /// unpurged because a queued op pins it, a flag change left unapplied
    /// because a queued op locks that flag, or a local row the pass had no
    /// trustworthy snapshot to reconcile against.
    applied_all: bool,
}

impl DeltaOutcome {
    fn new() -> Self {
        DeltaOutcome {
            flags_reconciled: 0,
            vanished: 0,
            applied_all: true,
        }
    }

    /// The server reports `msg_id` gone: purge it unless an op pins it.
    fn vanish(
        &mut self,
        db: &Database,
        locks: &OpLocks,
        account_id: &str,
        msg_id: &str,
    ) -> Result<(), CoreError> {
        if locks.pins(msg_id) {
            self.applied_all = false;
            return Ok(());
        }
        db.purge_message(account_id, msg_id).map_err(db_err)?;
        self.vanished += 1;
        Ok(())
    }

    /// Converge a locally-known message's `(read, starred)` flags to the
    /// server's `remote` values. A locked flag keeps its local value; that
    /// holds a change back only when the server's value actually differs (a
    /// lock whose flag already matches the server loses nothing).
    #[allow(clippy::too_many_arguments)]
    fn converge_flags(
        &mut self,
        db: &Database,
        locks: &OpLocks,
        account_id: &str,
        mailbox_id: &str,
        msg_id: &str,
        local: (bool, bool),
        remote: (bool, bool),
    ) -> Result<(), CoreError> {
        let read_differs = local.0 != remote.0;
        let star_differs = local.1 != remote.1;
        let read_lock = locks.read_lock(mailbox_id, msg_id);
        let star_lock = locks.star_lock(msg_id);
        if (read_lock && read_differs) || (star_lock && star_differs) {
            self.applied_all = false;
        }
        let apply_read = read_differs && !read_lock;
        let apply_star = star_differs && !star_lock;
        if apply_read || apply_star {
            let new_read = if apply_read { remote.0 } else { local.0 };
            let new_starred = if apply_star { remote.1 } else { local.1 };
            db.reconcile_message_flags(account_id, msg_id, new_read, new_starred)
                .map_err(db_err)?;
            self.flags_reconciled += 1;
        }
        Ok(())
    }

    /// A local row this pass could not reconcile at all (no trustworthy
    /// snapshot): whatever the server did to it is still unapplied.
    fn hold_back(&mut self) {
        self.applied_all = false;
    }

    /// Converge a message's keywords to the server-reported set (S42).
    /// `None` means the provider did not report keywords for this UID at
    /// all — never treated as "server has none", so local keyword state is
    /// left completely alone. A queued `keyword` op holds the change back
    /// exactly like a locked flag: the write is skipped, and the pass
    /// reports held-back data (via a cheap local-keyword comparison, since
    /// unlike `converge_flags` this has no resident copy of the current
    /// row) so the checkpoint rule never advances the modseq past it.
    fn converge_keywords(
        &mut self,
        db: &Database,
        locks: &OpLocks,
        msg_id: &str,
        keywords: Option<&Vec<String>>,
    ) -> Result<(), CoreError> {
        let Some(kw) = keywords else {
            return Ok(());
        };
        if self.keywords_held_back(db, locks, msg_id, kw)? {
            return Ok(());
        }
        if db.replace_server_keywords(msg_id, kw).map_err(db_err)? {
            self.flags_reconciled += 1;
        }
        Ok(())
    }

    /// The lock half of [`converge_keywords`]: true when a queued `keyword`
    /// op locks `msg_id`, in which case the write must be skipped. The pass
    /// is marked held-back only when the server's set actually differs from
    /// the server-origin rows already stored (local-only tags are never
    /// touched by a server report, so they are not a difference).
    fn keywords_held_back(
        &mut self,
        db: &Database,
        locks: &OpLocks,
        msg_id: &str,
        kw: &[String],
    ) -> Result<bool, CoreError> {
        if !locks.keyword_lock(msg_id) {
            return Ok(false);
        }
        let current = db.server_keyword_keys(msg_id).map_err(db_err)?;
        let remote: HashSet<String> = kw.iter().map(|k| normalise_keyword(k)).collect();
        if current != remote {
            self.applied_all = false;
        }
        Ok(true)
    }
}

/// Checkpoint rule (S30, review round 3): the cursor's HIGHESTMODSEQ may move
/// to the server's `reported` value only when every change up to it was
/// applied locally. Otherwise it keeps `baseline` — the previous modseq of
/// this same UID space, or `None` — so the next `SELECT..QRESYNC` (VANISHED
/// (EARLIER) + changed FETCH), or the full scan while it is `None`, reports
/// the held-back change again, and it applies once the pinning op resolves.
/// Advancing past it would lose it for good: QRESYNC never re-reports a
/// change older than the modseq the client asks from, and only sync — never
/// replay — calls `purge_message`, so a skipped expunge becomes a ghost row.
/// The cost of holding back is a larger delta while an op stays queued.
fn modseq_to_persist(
    baseline: Option<i64>,
    reported: Option<i64>,
    applied_all: bool,
) -> Option<i64> {
    if applied_all {
        reported
    } else {
        baseline
    }
}

/// The highest provider UID stored locally for a mailbox, if it has any
/// rows. Only used when the server gives no UIDNEXT, so the full local scan
/// is paid on that rare path alone.
fn highest_local_uid(
    db: &Database,
    account_id: &str,
    mailbox_id: &str,
) -> Result<Option<i64>, CoreError> {
    Ok(db
        .list_local_uid_flags(account_id, mailbox_id)
        .map_err(db_err)?
        .into_iter()
        .map(|(uid, _, _)| uid)
        .max())
}

/// Persist a mailbox's cursor, compare-and-set on the modseq (see
/// `Database::set_sync_cursor_if_modseq_is`). `expected` is the modseq this
/// sync run last read or wrote for the mailbox; it stops advancing the moment
/// a write finds the stored value changed — a local-only flag change
/// invalidated the baseline mid-run — so every later write in this run keeps
/// that invalidation too, and the next sync takes the full scan that
/// converges it. UIDVALIDITY/UIDNEXT are written either way.
fn persist_cursor(
    db: &Database,
    account_id: &str,
    mailbox_id: &str,
    state: &MailboxState,
    highest_modseq: Option<i64>,
    expected: &mut Option<i64>,
) -> Result<(), CoreError> {
    // Every call site reaches this only once a mailbox's descriptor step for
    // the pass has fully finished (chunked or not) — see `apply_descriptor_
    // chunk`'s doc comment. Always clears any in-progress chunk checkpoint
    //: a completed pass has nothing left to resume.
    // A server may omit UIDNEXT from SELECT (reported here as 0). Never
    // store that 0: the next pass would resume "after UID -1", i.e. fetch
    // everything again and treat it as new mail. Store the value the
    // completed import implies — one past the highest UID now held — or
    // nothing at all for a mailbox with no rows.
    let uid_next = if state.uid_next > 0 {
        Some(state.uid_next)
    } else {
        highest_local_uid(db, account_id, mailbox_id)?.map(|uid| uid + 1)
    };
    let cursor = SyncCursor {
        uid_validity: Some(state.uid_validity),
        uid_next,
        highest_modseq,
        descriptor_checkpoint_uid: None,
    };
    if db
        .set_sync_cursor_if_modseq_is(account_id, mailbox_id, &cursor, *expected)
        .map_err(db_err)?
    {
        *expected = highest_modseq;
    }
    Ok(())
}

/// Apply a `QresyncSync`'s delta to local state: flag changes on messages
/// still known locally, and purge of vanished UIDs — through the same
/// [`DeltaOutcome`] recorders, and so the same op-lock rules, as the full-scan
/// path, so a queued mark_read/flag/mark_all_read is never clobbered or purged
/// out from under the replay queue whichever strategy ran. The caller persists
/// the modseq via [`modseq_to_persist`].
fn apply_qresync_delta(
    db: &Database,
    account_id: &str,
    mailbox_id: &str,
    delta: &QresyncSync,
    locks: &OpLocks,
) -> Result<DeltaOutcome, CoreError> {
    let mut outcome = DeltaOutcome::new();
    if delta.changed.is_empty() && delta.vanished.is_empty() {
        return Ok(outcome);
    }
    let locals = db
        .list_local_uid_flags(account_id, mailbox_id)
        .map_err(db_err)?;
    let local_flags: HashMap<i64, (bool, bool)> = locals
        .iter()
        .map(|&(uid, read, starred)| (uid, (read, starred)))
        .collect();

    for changed in &delta.changed {
        // A changed UID with no local row has nothing to reconcile: the
        // descriptor fetch earlier in this run already imported every UID
        // past the cursor with current flags, and one it did not import
        // (below the S62 history window, or moved away locally) is
        // deliberately absent. Not a held-back change.
        let Some(local) = local_flags.get(&changed.uid) else {
            continue;
        };
        let msg_id = format!("{mailbox_id}:{}", changed.uid);
        outcome.converge_flags(
            db,
            locks,
            account_id,
            mailbox_id,
            &msg_id,
            *local,
            (changed.is_seen, changed.is_flagged),
        )?;
        outcome.converge_keywords(db, locks, &msg_id, changed.keywords.as_ref())?;
    }

    // VANISHED ranges may span far more UIDs than this mailbox ever held (one
    // 13-byte `1:4294967295` covers them all), so intersect them with the
    // rows this client actually has instead of walking them.
    for (uid, _, _) in &locals {
        if delta.vanished.contains(*uid) {
            outcome.vanish(db, locks, account_id, &format!("{mailbox_id}:{uid}"))?;
        }
    }

    Ok(outcome)
}

/// RFC 4549 descriptor reconciliation against a full `UID FETCH 1:* (FLAGS)`
/// snapshot: converge flag changes on existing messages and purge local UIDs
/// the server no longer has. An empty snapshot of a non-empty mailbox (a
/// genuinely empty one took the `exists == 0` branch) is untrustworthy: no
/// local row is purged, and each counts as held back.
fn reconcile_full_scan(
    db: &Database,
    account_id: &str,
    mailbox_id: &str,
    remote: &[UidFlags],
    locks: &OpLocks,
) -> Result<DeltaOutcome, CoreError> {
    let remote_map: HashMap<i64, (bool, bool, Option<&Vec<String>>)> = remote
        .iter()
        .map(|d| (d.uid, (d.is_seen, d.is_flagged, d.keywords.as_ref())))
        .collect();
    let mut outcome = DeltaOutcome::new();
    // Keyword sets to write, gathered over the whole pass and applied in ONE
    // transaction below (one keyword-map load + in-memory diff, not a lock +
    // SELECT + autocommit per local message).
    let mut pending_keywords: Vec<(String, &[String])> = Vec::new();
    for (uid, local_read, local_starred) in db
        .list_local_uid_flags(account_id, mailbox_id)
        .map_err(db_err)?
    {
        let msg_id = format!("{mailbox_id}:{uid}");
        match remote_map.get(&uid) {
            Some(&(is_seen, is_flagged, keywords)) => {
                outcome.converge_flags(
                    db,
                    locks,
                    account_id,
                    mailbox_id,
                    &msg_id,
                    (local_read, local_starred),
                    (is_seen, is_flagged),
                )?;
                if let Some(kw) = keywords {
                    if !outcome.keywords_held_back(db, locks, &msg_id, kw)? {
                        pending_keywords.push((msg_id, kw.as_slice()));
                    }
                }
            }
            None if !remote_map.is_empty() => outcome.vanish(db, locks, account_id, &msg_id)?,
            None => outcome.hold_back(),
        }
    }
    let items: Vec<(&str, &[String])> = pending_keywords
        .iter()
        .map(|(id, kw)| (id.as_str(), *kw))
        .collect();
    outcome.flags_reconciled += db
        .replace_server_keywords_batch(account_id, mailbox_id, &items)
        .map_err(db_err)?;
    Ok(outcome)
}

/// Parse the JSON config, connect an IMAP provider, sync, and return a JSON report.
///
/// Also maintains the durable account-level diagnostic row (migration 0016):
/// attempt at start, connect/sync failures as redacted kind+phrase+phase, and
/// success on completion. Diagnostic writes are best-effort — they never
/// change the sync result.
pub fn sync_imap_json(db: &Database, config_json: &str) -> Result<String, CoreError> {
    let cfg: ImapSyncConfig = serde_json::from_str(config_json)
        .map_err(|e| CoreError::Sync(format!("bad config json: {e}")))?;
    sync_imap_json_with(db, cfg, ImapProvider::connect)
}

/// `sync_imap_json` with the provider construction injected.
///
/// The seam exists so the CONNECT failure paths — including a panic inside
/// LOGIN/CAPABILITY — are testable without a hostile server: what matters is
/// that the account's diagnostic row records the failure, and that is exactly
/// what an unguarded panic used to skip.
fn sync_imap_json_with<P: MailProvider>(
    db: &Database,
    cfg: ImapSyncConfig,
    connect: impl FnOnce(&ImapConfig) -> Result<P, ProviderError>,
) -> Result<String, CoreError> {
    // Validated up front: a malformed window is a config bug, and must fail
    // before any network I/O or diagnostic write rather than mid-pass.
    let since = cfg
        .initial_sync_since
        .as_deref()
        .map(imap_since_date)
        .transpose()?;
    let _ = db.record_account_sync_attempt(&cfg.account_id);
    // Guarded THROUGH the connect, not just after it: connect runs LOGIN and
    // CAPABILITY, which are the very parser sites that panic. Wrapping only the
    // returned provider would let a panic there unwind past this match, so the
    // failure below would never be recorded against the account. Routed
    // through the shared `connect_guarded_with` factory — the same one
    // `connect_guarded_imap` uses — so there is exactly one place in the
    // crate that constructs a `PanicGuardedProvider`.
    let mut provider = match connect_guarded_with(
        connect,
        &ImapConfig {
            host: cfg.host.clone(),
            port: cfg.port,
            username: cfg.username.clone(),
            password: cfg.password.clone(),
            security: castellum_providers::TransportSecurity::resolve(
                cfg.imap_security.as_deref(),
                cfg.use_tls,
            ),
            allow_invalid_certs: cfg.allow_invalid_certs,
        },
    ) {
        Ok(provider) => provider,
        Err(e) => {
            let (kind, summary) = redact::redact_provider_error(&e);
            // `kind`/`summary` are already redacted for display/export (same
            // values written to the durable diagnostic row below) — safe to
            // log verbatim, unlike `e`'s own Display/Debug text.
            tracing::warn!(
                account_id = %cfg.account_id,
                kind = kind.as_str(),
                phase = phase::CONNECT,
                "sync: imap connect failed: {summary}"
            );
            let _ = db.record_account_sync_failure(
                &cfg.account_id,
                kind.as_str(),
                summary,
                phase::CONNECT,
            );
            // `sync_with_provider` never ran, so there is no in-flight
            // mailbox progress to finish — harmless no-op if nothing was
            // ever `begin()`-un for this account (e.g. its very first sync
            // attempt failing on connect).
            sync_progress::finish(&cfg.account_id);
            // F3: a first-ever connect failure has no accounts row, so the app
            // cannot read the kind back from local diagnostics. Carry it in the
            // thrown text as a leading "<kind>: " token (same convention as
            // send failures) so a wrong password is not shown as "can't reach".
            return Err(CoreError::Sync(crate::send::classified_provider_error(&e)));
        }
    };

    // Capture CAPABILITY tokens for diagnostics (sanitised protocol atoms).
    if let Some(caps) = provider.capabilities() {
        let tokens = redact::sanitize_capability_tokens(&caps);
        if let Ok(json) = serde_json::to_string(&tokens) {
            let _ = db.set_imap_capabilities(&cfg.account_id, &json);
        }
    }

    let report = record_and_run_sync(
        db,
        &cfg.account_id,
        &cfg.email,
        cfg.mailboxes.as_deref(),
        since.as_deref(),
        &mut provider,
        &cfg.host,
        // The sync upsert preserves the stored name; apply the configured
        // one (set or cleared on the Add/Edit sheet) now that the row
        // exists. Passed in as the `before_record` hook, not run after the
        // call (S31b review, finding d): on main it ran BEFORE the outcome
        // was recorded, so a failing upsert recorded neither success nor
        // failure. Done by the caller rather than inside
        // `record_and_run_sync` because an IDLE-woken resync has no config
        // to apply; the hook still runs for an abandoned pass (which
        // returns Ok), exactly the cases it did before S31.
        || {
            db.upsert_account(&Account {
                id: cfg.account_id.clone(),
                email: cfg.email.clone(),
                display_name: normalise_display_name(cfg.display_name.as_deref()),
                provider: "imap".to_string(),
            })
            .map_err(db_err)
        },
    )?;
    let _ = provider.logout();

    serde_json::to_string(&report).map_err(|e| CoreError::Sync(format!("serialize report: {e}")))
}

/// Run `sync_with_provider_scoped` against an already-connected,
/// already-guarded provider, wrapping it with the SAME account-level
/// diagnostic bookkeeping `sync_imap_json_with` has always done (the sync
/// *attempt* is recorded by the caller before connect; this records
/// success/failure and the session-abandoned special case). Shared so a
/// wake-triggered IDLE resync (`idle.rs`, S31) reports through
/// `account_sync_state` identically to a manual or periodic sync — the
/// account's health indicator doesn't care which trigger ran it. Does NOT
/// call `provider.logout()`: unlike a one-shot sync, an IDLE-triggered
/// resync must keep the connection alive to re-issue IDLE afterwards, so
/// logout stays the caller's own decision (`sync_imap_json_with` still does
/// it, immediately after this returns).
// Eight arguments: the seven that always travelled together plus the
// `before_record` hook (S31b); a parameter struct for two callers would only
// move the list.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_and_run_sync<P: MailProvider>(
    db: &Database,
    account_id: &str,
    email: &str,
    mailboxes_filter: Option<&[String]>,
    since_imap_date: Option<&str>,
    provider: &mut PanicGuardedProvider<P>,
    provider_host: &str,
    before_record: impl FnOnce() -> Result<(), CoreError>,
) -> Result<SyncReport, CoreError> {
    let report = match sync_with_provider_scoped(
        db,
        account_id,
        email,
        mailboxes_filter,
        since_imap_date,
        provider,
        provider_host,
    ) {
        Ok(report) => report,
        Err(e) => {
            let (kind, summary) = redact::redact_core_error(&e);
            tracing::warn!(
                account_id = %account_id,
                kind = kind.as_str(),
                phase = phase::SYNC,
                "sync: failed: {summary}"
            );
            let _ = db.record_account_sync_failure(account_id, kind.as_str(), summary, phase::SYNC);
            // A hard error (e.g. `?`-propagated DB failure) can bail out of
            // `sync_with_provider_scoped` mid-mailbox-loop, before its own
            // success path reaches `sync_progress::finish`. Mark it finished
            // here so a poller doesn't keep reading a stale "still syncing"
            // snapshot once this call has actually returned.
            sync_progress::finish(account_id);
            return Err(e);
        }
    };
    // A pass that completed but whose caller-side follow-up failed returns
    // that error with NO outcome recorded, as `sync_imap_json` did before
    // S31 (neither a success nor a failure row: the pass itself was fine, and
    // the failure is not a sync failure).
    before_record()?;
    // Checked BEFORE any logout (the caller's call, see above): a LOGOUT that
    // hits an already-dead socket closes the session, and must not turn a
    // completed pass into a recorded failure.
    // An abandoned session is an ACCOUNT-level failure even when the pass
    // returned Ok. `sync_with_provider_scoped` treats a mailbox failure as per-mailbox
    // and continues, which is right for permissions or a folder deleted
    // mid-run — but once the session is abandoned nothing more can sync, and
    // recording success here would clear `last_error_*` and reset
    // `consecutive_failures` to 0. The account would read healthy after exactly
    // the failure these diagnostics exist to catch. Two causes: the guard
    // caught a parser panic, or (review of PR #180, finding 2) an undecodable
    // response poisoned the IMAP session with no reconnect left to replace it
    // — `session_abandoned()` reports both, `is_abandoned()` only the first.
    if provider.session_abandoned() {
        let abandon_reason = provider.session_abandon_reason();
        tracing::error!(
            account_id = %account_id,
            phase = phase::SYNC,
            panic = provider.is_abandoned(),
            reason = ?abandon_reason,
            "sync: provider session abandoned; pass ended early, account marked failed"
        );
        let (kind, summary) = redact::redact_session_abandon_reason(abandon_reason);
        let _ = db.record_account_sync_failure(account_id, kind.as_str(), summary, phase::SYNC);
    } else {
        tracing::info!(account_id = %account_id, "sync: completed");
        let _ = db.record_account_sync_success(account_id);
    }
    Ok(report)
}

/// The IDLE supervisor's wake-triggered pass: `record_and_run_sync` on the
/// connection IDLE already holds (no logout, no initial-history window, no
/// caller follow-up), plus the one piece of cleanup the engine cannot do
/// without a profile directory — unlinking the cached attachment files a
/// folder removal orphaned. An IDLE pass runs
/// `remove_mailboxes_gone_from_server` like any other, even with its INBOX
/// filter, so without this the rows went and the bytes stayed (review of
/// #3456). Kept here rather than inline in `idle.rs` so the tests can drive
/// it with a scripted provider.
pub(crate) fn resync_on_live_connection<P: MailProvider>(
    profile_path: &std::path::Path,
    db: &Database,
    account_id: &str,
    email: &str,
    mailboxes_filter: Option<&[String]>,
    provider: &mut PanicGuardedProvider<P>,
    provider_host: &str,
) -> Result<SyncReport, CoreError> {
    let report = record_and_run_sync(
        db,
        account_id,
        email,
        mailboxes_filter,
        None,
        provider,
        provider_host,
        || Ok(()),
    )?;
    crate::attachments::unlink_orphaned_files(profile_path, &report.orphaned_attachment_hashes);
    Ok(report)
}

/// Persist an account's identity row (id/email/provider) WITHOUT performing a
/// sync — the "Save anyway" path in `AccountSetupSheet` for
/// when the initial connection check failed. Everywhere else, `accounts`
/// rows are only ever created as a side effect of a successful
/// `sync_with_provider` call, which never runs when the connect/login itself
/// failed (e.g. IMAP not yet enabled on the mail provider) — so without this
/// entry point the account never appears to `list_accounts`, even though the
/// user explicitly asked to keep it.
///
/// Deliberately does NOT touch `account_sync_state`: the failed connection
/// attempt that led here already called `record_account_sync_failure` (see
/// the `Err(e)` arm above), so the account is already correctly marked as
/// needing attention by the time this runs — reusing that existing
/// diagnostic state rather than inventing a second "unverified" concept.
pub fn save_account_without_sync_json(db: &Database, config_json: &str) -> Result<(), CoreError> {
    let cfg: ImapSyncConfig = serde_json::from_str(config_json)
        .map_err(|e| CoreError::Sync(format!("bad config json: {e}")))?;
    db.upsert_account(&Account {
        id: cfg.account_id,
        email: cfg.email,
        display_name: normalise_display_name(cfg.display_name.as_deref()),
        provider: "imap".to_string(),
    })
    .map_err(db_err)
}

/// Trim a user-entered display name; blank means "none" so the UI falls back
/// to the address.
fn normalise_display_name(name: Option<&str>) -> Option<String> {
    name.map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
}

/// Hydrate one descriptor-only message body by provider UID and return the
/// refreshed message detail as JSON. This is the body-on-open path: unlike
/// sync, it never scans a mailbox or fetches a batch.
pub fn hydrate_message_body_json(
    db: &Database,
    config_json: &str,
    message_id: &str,
) -> Result<String, CoreError> {
    let cfg: ImapSyncConfig = serde_json::from_str(config_json)
        .map_err(|e| CoreError::Sync(format!("bad config json: {e}")))?;

    let Some(detail) = db.get_message_detail(message_id).map_err(db_err)? else {
        return serialize_message_detail(&None);
    };
    if detail.header.account_id != cfg.account_id {
        return Err(CoreError::Sync(
            "message does not belong to configured account".into(),
        ));
    }
    if detail.header.has_body {
        return serialize_message_detail(&Some(detail));
    }

    // Guarded through the connect itself (LOGIN/CAPABILITY panic sites).
    let mut provider = connect_guarded_imap(&ImapConfig {
        host: cfg.host.clone(),
        port: cfg.port,
        username: cfg.username.clone(),
        password: cfg.password.clone(),
        security: castellum_providers::TransportSecurity::resolve(
            cfg.imap_security.as_deref(),
            cfg.use_tls,
        ),
        allow_invalid_certs: cfg.allow_invalid_certs,
    })
    .map_err(provider_err)?;

    let detail = hydrate_message_body_with_provider(
        db,
        &cfg.account_id,
        message_id,
        &mut provider,
        &cfg.host,
    )?;
    let _ = provider.logout();

    serialize_message_detail(&detail)
}

fn serialize_message_detail(detail: &Option<MessageDetail>) -> Result<String, CoreError> {
    serde_json::to_string(detail).map_err(|e| CoreError::Sync(format!("serialize detail: {e}")))
}

fn hydrate_message_body_with_provider(
    db: &Database,
    account_id: &str,
    message_id: &str,
    provider: &mut dyn MailProvider,
    provider_host: &str,
) -> Result<Option<MessageDetail>, CoreError> {
    let Some(detail) = db.get_message_detail(message_id).map_err(db_err)? else {
        return Ok(None);
    };
    if detail.header.account_id != account_id {
        return Err(CoreError::Sync(
            "message does not belong to configured account".into(),
        ));
    }
    if detail.header.has_body {
        return Ok(Some(detail));
    }

    let Some((mailbox_path, uid)) = db.message_imap_coords(message_id).map_err(db_err)? else {
        return Err(CoreError::Sync(format!(
            "message {message_id} has no provider uid"
        )));
    };

    let selected = provider.select(&mailbox_path).map_err(provider_err)?;
    let cursor = db
        .get_sync_cursor(account_id, &detail.header.mailbox_id)
        .map_err(db_err)?;
    let expected_uid_validity = cursor.and_then(|c| c.uid_validity);
    if selected.uid_validity <= 0
        || expected_uid_validity.is_none_or(|expected| expected != selected.uid_validity)
    {
        return Err(CoreError::Sync(
            "mailbox UIDVALIDITY changed; sync the mailbox before loading this body".into(),
        ));
    }
    let messages = provider.fetch_bodies(&[uid]).map_err(provider_err)?;
    let Some(rm) = messages.into_iter().find(|m| m.uid == uid) else {
        return Err(CoreError::Sync(format!(
            "provider did not return body for uid {uid}"
        )));
    };
    let (nm, extras) =
        new_message_from_raw(account_id, &detail.header.mailbox_id, rm, provider_host);
    db.upsert_message(&nm).map_err(db_err)?;
    db.set_unsubscribe_targets(
        message_id,
        extras.list_unsubscribe_mailto.as_deref(),
        extras.list_unsubscribe_url.as_deref(),
        extras.list_unsubscribe_one_click,
    )
    .map_err(db_err)?;
    db.upsert_message_trust(&nm.id, &extras.trust)
        .map_err(db_err)?;
    persist_calendar_invite(db, &nm.id, &extras)?;
    persist_message_headers(db, &nm.id, &extras);
    enqueue_embed_job(db, account_id, &nm.id);
    // Bug hunt #50: a parked arrival opened before the background pass got to it.
    let _ = crate::rules::drain_deferred_rules(db, account_id);
    db.get_message_detail(message_id).map_err(db_err)
}

/// Run one bounded background body-hydration pass and return its report as
/// JSON. Re-enters the durable missing-body queue: hydrated rows drop out of
/// the query, so repeated passes (and passes after a restart) resume exactly
/// where the last one stopped without any persisted hydration cursor. An empty
/// queue returns without opening a network connection.
pub fn hydrate_pending_bodies_json(db: &Database, config_json: &str) -> Result<String, CoreError> {
    let cfg: ImapSyncConfig = serde_json::from_str(config_json)
        .map_err(|e| CoreError::Sync(format!("bad config json: {e}")))?;

    let backlog = db.list_hydration_backlog(&cfg.account_id).map_err(db_err)?;
    let report = if backlog.is_empty() {
        HydrationReport {
            account_id: cfg.account_id,
            mailboxes: Vec::new(),
            requested: 0,
            hydrated: 0,
            bytes_fetched: 0,
            remaining: 0,
            elapsed_ms: 0,
        }
    } else {
        // Guarded through the connect itself (LOGIN/CAPABILITY panic sites).
        let mut provider = connect_guarded_imap(&ImapConfig {
            host: cfg.host.clone(),
            port: cfg.port,
            username: cfg.username.clone(),
            password: cfg.password.clone(),
            security: castellum_providers::TransportSecurity::resolve(
                cfg.imap_security.as_deref(),
                cfg.use_tls,
            ),
            allow_invalid_certs: cfg.allow_invalid_certs,
        })
        .map_err(provider_err)?;
        let byte_budget = cfg
            .hydration_byte_budget
            .unwrap_or(HYDRATION_PASS_DEFAULT_BYTE_BUDGET)
            .max(0);
        let report = hydrate_pending_bodies_with_provider(
            db,
            &cfg.account_id,
            &mut provider,
            byte_budget,
            &cfg.host,
        )?;
        let _ = provider.logout();
        report
    };

    serde_json::to_string(&report)
        .map_err(|e| CoreError::Sync(format!("serialize hydration report: {e}")))
}

/// Mailbox visit order for a hydration pass: Inbox-like folders first so
/// recent interactive mail becomes readable soonest; junk/trash last.
fn hydration_role_rank(role: Option<&str>) -> u8 {
    match role {
        Some("inbox") => 0,
        Some("drafts") => 1,
        Some("sent") => 2,
        Some("archive") => 3,
        Some("junk") => 5,
        Some("trash") => 6,
        _ => 4,
    }
}

/// One bounded hydration pass, provider-agnostic (so tests can drive a mock).
///
/// Per mailbox with backlog, in priority order: select, require the same
/// positive UIDVALIDITY as the stored cursor (mirroring body-on-open), fetch
/// one bounded newest-first batch of missing bodies, and persist it. Budgets:
/// at most [`BODY_HYDRATION_BATCH_LIMIT`] bodies per mailbox,
/// [`HYDRATION_PASS_TOTAL_LIMIT`] per pass, and `byte_budget` predicted
/// bytes per pass — candidates are selected against the remaining byte
/// budget from stored RFC822.SIZE metadata BEFORE any fetch, so the budget
/// is a real pre-fetch bound (up to server size honesty), not a post-pass
/// stop threshold. Any per-mailbox failure is recorded in the report and
/// later mailboxes continue.
fn hydrate_pending_bodies_with_provider(
    db: &Database,
    account_id: &str,
    provider: &mut dyn MailProvider,
    byte_budget: i64,
    provider_host: &str,
) -> Result<HydrationReport, CoreError> {
    let pass_start = std::time::Instant::now();
    let mut backlog = db.list_hydration_backlog(account_id).map_err(db_err)?;
    backlog.sort_by(|a, b| {
        hydration_role_rank(a.role.as_deref())
            .cmp(&hydration_role_rank(b.role.as_deref()))
            .then(b.last_activity.cmp(&a.last_activity))
            .then(a.path.cmp(&b.path))
    });

    let mut mailboxes = Vec::new();
    let mut requested_total = 0usize;
    let mut hydrated_total = 0usize;
    let mut bytes_total = 0i64;
    let mut remaining_total = 0i64;
    let mut budget = HYDRATION_PASS_TOTAL_LIMIT;
    let mut bytes_budget_left = byte_budget;

    for entry in &backlog {
        if budget <= 0 || bytes_budget_left <= 0 {
            remaining_total += entry.missing;
            continue;
        }
        let mb_start = std::time::Instant::now();
        let mut visit = MailboxHydrationReport {
            path: entry.path.clone(),
            requested: 0,
            hydrated: 0,
            bytes_fetched: 0,
            duration_ms: 0,
            remaining: entry.missing,
            error: None,
        };

        match hydrate_mailbox_batch(
            db,
            account_id,
            entry,
            provider,
            budget,
            bytes_budget_left,
            provider_host,
        ) {
            Ok((requested, hydrated, bytes, budgeted)) => {
                visit.requested = requested;
                visit.hydrated = hydrated;
                visit.bytes_fetched = bytes;
                // Clamp: a concurrent sync can import new descriptor rows
                // between the backlog snapshot and this batch, so `hydrated`
                // may exceed the snapshot count. A negative value would
                // deflate the account total and stop callers too early.
                visit.remaining = (entry.missing - hydrated as i64).max(0);
                budget -= requested as i64;
                // Charge the larger of predicted and actual bytes so a
                // server under-reporting RFC822.SIZE still consumes budget.
                bytes_budget_left -= budgeted.max(bytes);
            }
            Err(e) => visit.error = Some(e.to_string()),
        }

        visit.duration_ms = mb_start.elapsed().as_millis() as u64;
        requested_total += visit.requested;
        hydrated_total += visit.hydrated;
        bytes_total += visit.bytes_fetched;
        remaining_total += visit.remaining;
        mailboxes.push(visit);
    }

    // Bug hunt #50: arrivals parked for a Body/Attachment rule may have their
    // body now. Best effort: a rules failure never fails a hydration pass.
    if hydrated_total > 0 {
        let _ = crate::rules::drain_deferred_rules(db, account_id);
    }

    Ok(HydrationReport {
        account_id: account_id.to_string(),
        mailboxes,
        requested: requested_total,
        hydrated: hydrated_total,
        bytes_fetched: bytes_total,
        remaining: remaining_total,
        elapsed_ms: pass_start.elapsed().as_millis() as u64,
    })
}

/// Hydrate one bounded batch for one mailbox. Returns (requested, hydrated,
/// actual bytes, budgeted bytes). Errors here are per-mailbox: the caller
/// records them and moves on.
fn hydrate_mailbox_batch(
    db: &Database,
    account_id: &str,
    entry: &castellum_models::MailboxBodyBacklog,
    provider: &mut dyn MailProvider,
    budget: i64,
    byte_budget: i64,
    provider_host: &str,
) -> Result<(usize, usize, i64, i64), CoreError> {
    // Pre-fetch byte selection: the query already excludes rows whose known
    // size alone exceeds the remaining byte budget (so one oversized body
    // never starves smaller ones behind it in the newest-first order), then
    // the running total trims the batch. Skipped-oversized rows stay in the
    // durable queue for explicit body-on-open hydration. Selection runs
    // BEFORE the network SELECT so a mailbox with nothing that fits costs
    // no provider round-trip.
    let candidates = db
        .list_body_missing_uids_with_sizes(
            account_id,
            &entry.mailbox_id,
            BODY_HYDRATION_BATCH_LIMIT.min(budget),
            byte_budget,
        )
        .map_err(db_err)?;
    let mut batch = Vec::new();
    let mut budgeted = 0i64;
    for (uid, size) in candidates {
        let predicted = size.unwrap_or(HYDRATION_UNKNOWN_SIZE_ESTIMATE);
        // Saturating: the budget is caller-supplied i64; a hostile or buggy
        // huge value must not wrap the comparison into admitting the row.
        if budgeted.saturating_add(predicted) > byte_budget {
            // Running budget spent; a smaller candidate later in the
            // newest-first order may still fit.
            continue;
        }
        budgeted = budgeted.saturating_add(predicted);
        batch.push(uid);
    }
    if batch.is_empty() {
        return Ok((0, 0, 0, 0));
    }

    let selected = provider.select(&entry.path).map_err(provider_err)?;
    let cursor_uid_validity = |db: &Database| -> Result<Option<i64>, CoreError> {
        Ok(db
            .get_sync_cursor(account_id, &entry.mailbox_id)
            .map_err(db_err)?
            .and_then(|c| c.uid_validity))
    };
    let expected = cursor_uid_validity(db)?;
    if selected.uid_validity <= 0 || expected.is_none_or(|e| e != selected.uid_validity) {
        return Err(CoreError::Sync(
            "mailbox UIDVALIDITY changed; sync the mailbox before hydrating bodies".into(),
        ));
    }

    let messages = provider.fetch_bodies(&batch).map_err(provider_err)?;

    // A concurrent sync may have detected a UIDVALIDITY reset and purged this
    // mailbox while the fetch ran; the fetched bodies would then belong to the
    // old UID space. Re-check the stored cursor and drop the batch if identity
    // changed (body-on-open has the same guard one UID wide).
    if cursor_uid_validity(db)? != Some(selected.uid_validity) {
        return Err(CoreError::Sync(
            "mailbox UIDVALIDITY changed during body fetch; batch dropped".into(),
        ));
    }

    let wanted: HashSet<i64> = batch.iter().copied().collect();
    let mut hydrated = 0usize;
    let mut bytes = 0i64;
    for rm in messages {
        if !wanted.contains(&rm.uid) {
            continue;
        }
        bytes += rm.raw.len() as i64;
        let (nm, extras) = new_message_from_raw(account_id, &entry.mailbox_id, rm, provider_host);
        db.upsert_message(&nm).map_err(db_err)?;
        db.set_unsubscribe_targets(
            &nm.id,
            extras.list_unsubscribe_mailto.as_deref(),
            extras.list_unsubscribe_url.as_deref(),
            extras.list_unsubscribe_one_click,
        )
        .map_err(db_err)?;
        db.upsert_message_trust(&nm.id, &extras.trust)
            .map_err(db_err)?;
        persist_calendar_invite(db, &nm.id, &extras)?;
        persist_message_headers(db, &nm.id, &extras);
        enqueue_embed_job(db, account_id, &nm.id);
        hydrated += 1;
    }
    Ok((batch.len(), hydrated, bytes, budgeted))
}

/// Core sync algorithm, provider-agnostic (so tests can drive a mock).
/// No initial-history window — see [`sync_with_provider_scoped`].
pub fn sync_with_provider(
    db: &Database,
    account_id: &str,
    email: &str,
    mailboxes_filter: Option<&[String]>,
    provider: &mut dyn MailProvider,
    provider_host: &str,
) -> Result<SyncReport, CoreError> {
    sync_with_provider_scoped(
        db,
        account_id,
        email,
        mailboxes_filter,
        None,
        provider,
        provider_host,
    )
}

/// [`sync_with_provider`] with an optional initial-history window (S62).
/// `since_imap_date` is an RFC 3501 date from [`imap_since_date`]; it only
/// affects a mailbox's first-ever import (no cursor row) — see
/// `ImapSyncConfig::initial_sync_since`.
pub fn sync_with_provider_scoped(
    db: &Database,
    account_id: &str,
    email: &str,
    mailboxes_filter: Option<&[String]>,
    since_imap_date: Option<&str>,
    provider: &mut dyn MailProvider,
    provider_host: &str,
) -> Result<SyncReport, CoreError> {
    // S31b: one pass per account at a time, whatever triggered it. An IDLE
    // wake resyncs inside Rust, where Swift's coalescing cannot see it; a
    // second pass overlapping the first would import the same UID as new
    // twice and fire arrival rules / mute-on-arrival twice. A trigger that
    // arrives meanwhile waits here and runs as the follow-up pass.
    let _pass = pass_gate::acquire(pass_gate::scope_of(db), account_id);
    // Keep a display name already stored for this account (set from the
    // connection config by `sync_imap_json`); this entry point has no name.
    let stored_name = db
        .list_accounts()
        .map_err(db_err)?
        .into_iter()
        .find(|a| a.id == account_id)
        .and_then(|a| a.display_name);
    db.upsert_account(&Account {
        id: account_id.to_string(),
        email: email.to_string(),
        display_name: stored_name,
        provider: "imap".to_string(),
    })
    .map_err(db_err)?;

    let remote = sync_timing::time("imap.list_mailboxes", || provider.list_mailboxes())
        .map_err(provider_err)?;
    // Progress tracking: reset the snapshot now that the
    // mailbox count for this pass is known. See `sync_progress` for why this
    // is in-process/best-effort rather than a durable/pushed signal.
    sync_progress::begin(account_id, remote.len());
    let canonical_roles = canonical_role_paths(&remote);
    let mut reports = Vec::new();
    // S40: ids of messages genuinely NEW in this sync pass (never ids that
    // merely had their body hydrated for an already-known message — see the
    // `missing_body_uids` block below, which deliberately does not push
    // here). Its `for_rules` half is handed to one
    // `rules::run_rules_for_account` call at the end of the pass, batched
    // rather than per-message (Assumption 3); see `NewArrivals` for which
    // mail qualifies.
    let mut new_message_ids = NewArrivals::default();
    // S59: mailboxes whose cursor was warm (same UIDVALIDITY as the stored one)
    // when this pass selected them. Only arrivals in these can be announced:
    // a first sync or a UIDVALIDITY reset "discovers" the whole mailbox as new.
    let mut warm_mailboxes: HashSet<String> = HashSet::new();
    // The decode-skip retry allowance for this whole pass (see
    // `RETRY_UIDS_PER_PASS`), spent mailbox by mailbox.
    let mut retry_uids_left = RETRY_UIDS_PER_PASS;

    // Reconciliation must not clobber un-replayed local intent: build the
    // op guards from the pending operation queue (see `OpLocks`), and top
    // them up before each mailbox's reconciliation step (`OpLocks::refresh`)
    // so an op queued while the pass is running is honoured too.
    let pending = db.pending_operations(account_id).map_err(db_err)?;
    let mut locks = OpLocks::from_pending(&pending);

    let explicit_filter = mailboxes_filter.is_some();
    'mailbox: for rmb in &remote {
        // A session abandoned while syncing the PREVIOUS mailbox — by a
        // call that does not check for it itself (a body fetch, a flag
        // scan, the legacy full fetch) — must not be used for this one's
        // SELECT: that would record a bogus SELECT failure against an
        // innocent mailbox before the loop noticed (review of PR #180,
        // round 3).
        if provider.session_abandoned() {
            break 'mailbox;
        }
        // Marks this mailbox visited in the progress snapshot no matter which
        // of the loop body's many exits is taken below (skip, select error,
        // fetch error, `continue`, `break`, or falling through to the report
        // push at the bottom) — a `Drop` guard is the only way to cover every
        // one of those without duplicating `sync_progress::finish_mailbox`
        // at each site. See `sync_progress` for the rest of this pass's
        // tracking (started per-mailbox further down, once SELECT succeeds).
        struct MailboxProgressGuard<'a>(&'a str);
        impl Drop for MailboxProgressGuard<'_> {
            fn drop(&mut self) {
                sync_progress::finish_mailbox(self.0);
            }
        }
        let _progress_guard = MailboxProgressGuard(account_id);
        // Never inherit the previous folder's arrival verdict (set below,
        // once this folder's cursor and role are known).
        new_message_ids.rules_eligible = false;
        if let Some(filter) = mailboxes_filter {
            if !filter.iter().any(|p| p.eq_ignore_ascii_case(&rmb.path)) {
                continue;
            }
        }

        let mailbox_id = format!("{account_id}:{}", rmb.path);
        let role = effective_role(rmb, &canonical_roles);
        let sync_by_default = should_sync_by_default(rmb, role.as_deref());
        db.upsert_mailbox(&Mailbox {
            id: mailbox_id.clone(),
            account_id: account_id.to_string(),
            path: rmb.path.clone(),
            name: rmb.name.clone(),
            role: role.clone(),
            delimiter: rmb.delimiter.clone(),
            subscribed: rmb.subscribed,
            selectable: rmb.selectable,
            sync_by_default,
        })
        .map_err(db_err)?;

        // Container-only (`\Noselect`) folders hold no messages and error on
        // SELECT — record them for the hierarchy but never try to fetch them.
        if !rmb.selectable {
            let total = db.count_messages(account_id, &mailbox_id).map_err(db_err)?;
            reports.push(MailboxSyncReport {
                path: rmb.path.clone(),
                fetched: 0,
                descriptors_fetched: 0,
                bodies_fetched: 0,
                bytes_fetched: 0,
                fetched_uid_min: None,
                fetched_uid_max: None,
                total,
                cancelled_ops: 0,
                flags_reconciled: 0,
                vanished: 0,
                descriptors_skipped: 0,
                error: None,
                skipped: Some("non-selectable".into()),
            });
            continue;
        }

        if !explicit_filter && !sync_by_default {
            let total = db.count_messages(account_id, &mailbox_id).map_err(db_err)?;
            reports.push(MailboxSyncReport {
                path: rmb.path.clone(),
                fetched: 0,
                descriptors_fetched: 0,
                bodies_fetched: 0,
                bytes_fetched: 0,
                fetched_uid_min: None,
                fetched_uid_max: None,
                total,
                cancelled_ops: 0,
                flags_reconciled: 0,
                vanished: 0,
                descriptors_skipped: 0,
                error: None,
                skipped: Some("not in default sync set".into()),
            });
            continue;
        }

        // The mailbox row exists from here on, so the durable diagnostic row
        // (migration 0016) can track this attempt. Best-effort: diagnostic
        // writes never change sync behaviour.
        let _ = db.record_mailbox_sync_attempt(account_id, &mailbox_id);

        // Read the cursor BEFORE selecting: QRESYNC needs the previous
        // uid_validity/highest_modseq to ask the server for a delta, and a
        // plain select() below still benefits from knowing whether one
        // exists (unchanged from before this task, just moved earlier).
        let cursor = db
            .get_sync_cursor(account_id, &mailbox_id)
            .map_err(db_err)?;
        let qresync_eligible = provider.qresync_available()
            && cursor.as_ref().is_some_and(|c| {
                c.uid_validity.is_some() && c.highest_modseq.is_some_and(|m| m > 0)
            });

        // A per-mailbox SELECT failure (permissions, a folder deleted server-side
        // mid-run, a provider quirk) must NOT abort the whole account sync —
        // record it and move on so every other folder still syncs.
        let qresync_outcome = if qresync_eligible {
            let c = cursor.as_ref().expect("checked by qresync_eligible");
            match sync_timing::time("imap.select_qresync", || {
                provider.select_qresync(
                    &rmb.path,
                    c.uid_validity.expect("checked by qresync_eligible"),
                    c.highest_modseq.expect("checked by qresync_eligible"),
                )
            }) {
                // A UIDVALIDITY mismatch means the server ignored our QRESYNC
                // parameters (RFC 7162 3.6.2) and did an ordinary SELECT — the
                // existing reset-path comparison below already handles that
                // correctly when qresync_outcome is None, so don't trust this
                // delta.
                Ok(outcome) if Some(outcome.state.uid_validity) == c.uid_validity => Some(outcome),
                Ok(_) => None,
                Err(ProviderError::Unsupported(_)) => None,
                Err(e) => {
                    let (kind, summary) = redact::redact_provider_error(&e);
                    let _ = db.record_mailbox_sync_failure(
                        account_id,
                        &mailbox_id,
                        kind.as_str(),
                        summary,
                        phase::SELECT,
                    );
                    let total = db.count_messages(account_id, &mailbox_id).map_err(db_err)?;
                    reports.push(MailboxSyncReport {
                        path: rmb.path.clone(),
                        fetched: 0,
                        descriptors_fetched: 0,
                        bodies_fetched: 0,
                        bytes_fetched: 0,
                        fetched_uid_min: None,
                        fetched_uid_max: None,
                        total,
                        cancelled_ops: 0,
                        flags_reconciled: 0,
                        vanished: 0,
                        descriptors_skipped: 0,
                        error: Some(e.to_string()),
                        skipped: None,
                    });
                    if provider.session_abandoned() {
                        break;
                    }
                    continue;
                }
            }
        } else {
            None
        };

        let state = if let Some(outcome) = &qresync_outcome {
            outcome.state
        } else {
            match sync_timing::time("imap.select", || provider.select(&rmb.path)) {
                Ok(state) => state,
                Err(e) => {
                    let (kind, summary) = redact::redact_provider_error(&e);
                    let _ = db.record_mailbox_sync_failure(
                        account_id,
                        &mailbox_id,
                        kind.as_str(),
                        summary,
                        phase::SELECT,
                    );
                    let total = db.count_messages(account_id, &mailbox_id).map_err(db_err)?;
                    reports.push(MailboxSyncReport {
                        path: rmb.path.clone(),
                        fetched: 0,
                        descriptors_fetched: 0,
                        bodies_fetched: 0,
                        bytes_fetched: 0,
                        fetched_uid_min: None,
                        fetched_uid_max: None,
                        total,
                        cancelled_ops: 0,
                        flags_reconciled: 0,
                        vanished: 0,
                        descriptors_skipped: 0,
                        error: Some(e.to_string()),
                        skipped: None,
                    });
                    // A desynchronised session cannot recover: every remaining
                    // mailbox would fail identically against the abandoned guard.
                    // Stop instead of recording N pointless failures.
                    if provider.session_abandoned() {
                        break;
                    }
                    continue;
                }
            }
        };
        sync_progress::start_mailbox(account_id, &rmb.path, state.exists);

        // The cursor still describes this UID space only when UIDVALIDITY is
        // known (> 0) and unchanged; otherwise the server reset it (or it is
        // unknown), handled below by purging and refetching.
        let same_uid_space = cursor
            .as_ref()
            .is_some_and(|c| state.uid_validity > 0 && c.uid_validity == Some(state.uid_validity));
        // An earlier import COMPLETED in this UID space: `persist_cursor`
        // wrote a real UIDNEXT. An interrupted first import leaves only a
        // chunk checkpoint (`uid_next` NULL), and a UIDVALIDITY reset NULLs
        // it in the purge transaction, so resuming either is still history,
        // not arrivals. One verdict for both gates below (notifications via
        // `warm_mailboxes`, arrival rules via `rules_eligible`) so they
        // cannot drift (bug hunt 2026-10-12, #3456: the notification gate
        // used `same_uid_space` alone and announced resumed history). A
        // stored 0 is an old cursor from a server that omits UIDNEXT, never
        // a completed-import marker.
        let completed_import = same_uid_space
            && cursor
                .as_ref()
                .is_some_and(|c| c.uid_next.is_some_and(|n| n > 0));
        if completed_import {
            warm_mailboxes.insert(mailbox_id.clone());
        }
        // The two HIGHESTMODSEQ candidates of the checkpoint rule (see
        // `modseq_to_persist`): the previous cursor's value — only while the
        // UID space is unchanged, since a modseq from a reset space describes
        // a different mailbox and must never be persisted against the new
        // UIDVALIDITY — and the value this SELECT reported, captured now so it
        // pairs with the state it came with.
        let baseline_modseq = if same_uid_space {
            cursor.as_ref().and_then(|c| c.highest_modseq)
        } else {
            None
        };
        // A QRESYNC outcome's own value only: falling back to
        // `last_highest_modseq()` there could pick up a provider's stale
        // value from another mailbox.
        let reported_modseq = match &qresync_outcome {
            Some(outcome) => outcome.highest_modseq,
            None => provider.last_highest_modseq(),
        };
        // What this run believes is stored, for the compare-and-set cursor
        // writes below (`persist_cursor`).
        let mut modseq_expected = cursor.as_ref().and_then(|c| c.highest_modseq);

        // Empty mailbox: nothing to fetch (and "1:*" can error on some servers).
        // Two ways to reach exists == 0, handled distinctly:
        //   * `!same_uid_space` → the server reset its UID space to empty, OR
        // there is NO cursor at all yet local rows may
        //     already exist — an earlier first import that fetched rows and
        //     was then interrupted before `persist_cursor` ever ran. Neither
        //     case has a trustworthy recorded UIDVALIDITY for those rows (a
        //     missing cursor records none at all), so both are treated
        //     identically to the reset path: purge ALL local rows AND cancel
        //     stale UID-bound ops, because UIDs can be reused in the new
        //     space and a stale mark_read/flag would hit the wrong message
        //     once one is reassigned. For a genuine first-ever import with no
        //     local rows yet, this is a harmless no-op.
        //   * `same_uid_space` → same UID space, mailbox genuinely emptied.
        //     Purge local rows so the view converges, but keep any row still pinned
        //     by a queued per-message op so replay can resolve it first.
        if state.exists == 0 {
            // Nothing is on the server, so nothing is left to bring back (#69).
            db.drop_pending_refetch(account_id, &rmb.path)
                .map_err(db_err)?;
            let uidvalidity_reset = !same_uid_space;
            let mut cancelled_ops = 0usize;
            let mut outcome = DeltaOutcome::new();
            if uidvalidity_reset {
                db.delete_messages_for_mailbox(account_id, &mailbox_id)
                    .map_err(db_err)?;
                cancelled_ops = db
                    .cancel_operations_for_purged_mailbox(
                        account_id,
                        &mailbox_id,
                        &rmb.path,
                        &format!("cancelled: UIDVALIDITY reset purged mailbox {}", rmb.path),
                    )
                    .map_err(db_err)?;
            } else {
                locks.refresh(db, account_id)?;
                for (uid, _, _) in db
                    .list_local_uid_flags(account_id, &mailbox_id)
                    .map_err(db_err)?
                {
                    outcome.vanish(db, &locks, account_id, &format!("{mailbox_id}:{uid}"))?;
                }
            }
            let new_highest_modseq =
                modseq_to_persist(baseline_modseq, reported_modseq, outcome.applied_all);
            persist_cursor(
                db,
                account_id,
                &mailbox_id,
                &state,
                new_highest_modseq,
                &mut modseq_expected,
            )?;
            let _ = db.record_mailbox_sync_success(account_id, &mailbox_id);
            let total = db.count_messages(account_id, &mailbox_id).map_err(db_err)?;
            reports.push(MailboxSyncReport {
                path: rmb.path.clone(),
                fetched: 0,
                descriptors_fetched: 0,
                bodies_fetched: 0,
                bytes_fetched: 0,
                fetched_uid_min: None,
                fetched_uid_max: None,
                total,
                cancelled_ops,
                flags_reconciled: 0,
                vanished: outcome.vanished,
                descriptors_skipped: 0,
                error: None,
                skipped: None,
            });
            continue;
        }

        // Resume from the cursor only in the same UID space. Otherwise
        // (`!same_uid_space`, which is also true whenever there is no cursor
        // at all — see the `exists == 0` branch above for why a missing
        // cursor is folded into the reset path): the server's
        // UID space was reset, is unknown, or an earlier first import was
        // interrupted before ever writing a cursor. Purge the mailbox's
        // local rows and refetch from scratch, so stale messages from the
        // old (or unrecorded) UID space don't linger — and cancel any queued
        // ops that reference the purged messages' UIDs, which would
        // otherwise no-op later or (theoretically) hit re-assigned UIDs. A
        // genuine first-ever import has no local rows or ops yet, so this is
        // a harmless no-op for it.
        let mut cancelled_ops = 0usize;
        let after_uid = if same_uid_space {
            match cursor.as_ref().and_then(|c| c.uid_next).filter(|n| *n > 0) {
                Some(uid_next) => Some(uid_next - 1),
                // No usable stored UIDNEXT. While the server reports one,
                // that is a resumed interrupted import and the chunk
                // checkpoint drives it (start from scratch here). When the
                // server OMITS UIDNEXT (reported as 0; RFC 3501 allows it)
                // there is no chunking and no checkpoint, and `None` would
                // re-import the whole mailbox as new mail on every pass
                // (bug hunt 2026-10-12, #3456) — resume after the highest
                // UID already stored instead. This also repairs a cursor an
                // older build stored as 0.
                None if state.uid_next <= 0 => highest_local_uid(db, account_id, &mailbox_id)?,
                None => None,
            }
        } else {
            // Purge AND reset the cursor to the new UID space in one
            // transaction (Opus review of PR #180): leaving the cursor
            // row's `uid_validity` at its old value here let a resumed
            // chunked import's checkpoint disagree with it, so the NEXT
            // pass's `same_uid_space` check would purge again forever. See
            // `Database::purge_mailbox_for_uid_space_reset`'s doc comment.
            cancelled_ops = db
                .purge_mailbox_for_uid_space_reset(
                    account_id,
                    &mailbox_id,
                    &rmb.path,
                    state.uid_validity,
                )
                .map_err(db_err)?;
            // The reset above already wrote `highest_modseq = NULL` for the
            // cursor row, so the in-memory baseline this pass's later
            // `persist_cursor` compare-and-sets against must follow —
            // otherwise it still expects the OLD space's value, the stored
            // NULL never matches it, and the new space's modseq can never
            // be written (Opus review of PR #180 fix uncovered this).
            modseq_expected = None;
            None
        };

        // Initial-history window (S62). ONLY for a mailbox's first-ever
        // import (no cursor row at all). Deliberately NOT after a UIDVALIDITY
        // reset: that path just purged mail the user already had locally —
        // possibly years of it, imported via the cursor long after the
        // window was chosen — so re-applying the window there would silently
        // lose it. A reset keeps the pre-S62 full re-import. For a first
        // import: look up the
        // lowest UID on/after the window date and start from there. With no
        // message that recent, fetch nothing — the cursor below still
        // advances to UIDNEXT, so only NEW mail arrives from then on. A
        // provider without SEARCH support falls back to the full import (the
        // pre-S62 behaviour); any other search error fails this mailbox for
        // this pass rather than silently downloading history the user
        // declined — the next sync retries the window.
        let mut skip_fetch = false;
        let first_ever_import = cursor.is_none();
        // MM-3 review: arrival rules see this folder's new mail only when it
        // is the Inbox and this is an incremental pass over a UID space that
        // an earlier import COMPLETED (`persist_cursor` writes `uid_next`;
        // an interrupted first import leaves only a checkpoint, and a reset
        // is `!same_uid_space`). Decided before any fetch, so every path
        // below — chunked, sparse, legacy, and the early failure exits —
        // records the same verdict.
        new_message_ids.rules_eligible = role.as_deref() == Some("inbox") && completed_import;
        let after_uid = match (after_uid, since_imap_date) {
            (None, Some(date)) if first_ever_import => match provider.first_uid_since(date) {
                Ok(Some(uid)) => Some(uid - 1),
                Ok(None) => {
                    skip_fetch = true;
                    None
                }
                Err(ProviderError::Unsupported(_)) => None,
                Err(e) => {
                    let (kind, summary) = redact::redact_provider_error(&e);
                    let _ = db.record_mailbox_sync_failure(
                        account_id,
                        &mailbox_id,
                        kind.as_str(),
                        summary,
                        phase::FETCH,
                    );
                    let total = db.count_messages(account_id, &mailbox_id).map_err(db_err)?;
                    reports.push(MailboxSyncReport {
                        path: rmb.path.clone(),
                        fetched: 0,
                        descriptors_fetched: 0,
                        bodies_fetched: 0,
                        bytes_fetched: 0,
                        fetched_uid_min: None,
                        fetched_uid_max: None,
                        total,
                        cancelled_ops,
                        flags_reconciled: 0,
                        vanished: 0,
                        descriptors_skipped: 0,
                        error: Some(e.to_string()),
                        skipped: None,
                    });
                    if provider.session_abandoned() {
                        break;
                    }
                    continue;
                }
            },
            (after_uid, _) => after_uid,
        };

        // Resume a chunked descriptor import from its own
        // checkpoint, not from `after_uid` alone: a prior pass may have
        // committed chunks past `after_uid` (the S62-window / incremental
        // resume point) without ever reaching `persist_cursor` — an
        // interrupt, or simply the account quitting mid-import. Only trusted
        // in the SAME UID space as the checkpoint (`same_uid_space`); a reset
        // already purged local rows above and starts the chunk loop from
        // scratch like any other from-scratch import.
        let after_uid = match cursor.as_ref().and_then(|c| c.descriptor_checkpoint_uid) {
            Some(checkpoint) if same_uid_space => {
                Some(after_uid.map_or(checkpoint, |a| a.max(checkpoint)))
            }
            _ => after_uid,
        };

        // Chunked descriptor fetch: ~DESCRIPTOR_CHUNK_SIZE
        // UIDs per `UID FETCH`, lowest-first so the checkpoint below advances
        // monotonically, each chunk persisted through the same #178 batch
        // path and checkpointed durably before the next chunk is requested —
        // an interrupt resumes from the last COMMITTED chunk, never from UID
        // 1. Only attempted when the SELECT already told us the upper bound
        // (`state.uid_next`); a provider/server that didn't report it falls
        // back to the legacy unbounded fetch below, same as a provider that
        // returns `Unsupported` for the very first chunk (checked before any
        // write, so falling back here is always safe — nothing has been
        // persisted for this fetch yet).
        let mut descriptors_skipped = 0usize;
        let chunked_descriptors: Option<ChunkedDescriptorFetch> = if skip_fetch {
            None
        } else if state.uid_next > 0 {
            let hi = state.uid_next - 1;
            let low_start = after_uid.map(|u| u + 1).unwrap_or(1).max(1);
            let mut acc = ChunkedDescriptorFetch::default();
            let mut unsupported = false;

            //, Opus review point 2: a sparse mailbox (e.g. a
            // heavily-expunged Spam folder with a few hundred real messages
            // spread across a UIDNEXT in the millions) must not walk the UID
            // NUMBER SPACE in DESCRIPTOR_CHUNK_SIZE windows — most would come
            // back empty, at one real round trip each, on every incremental
            // sync. When the span is bigger than one chunk, search once for
            // the UIDs that actually exist and group THOSE into chunks
            // instead. A span that already fits in one chunk skips the
            // search: there is nothing to gain from it. Any search failure
            // (unsupported, or a real error) falls back to walking the UID
            // range directly below — this is strictly an optimisation, and a
            // provider/server that can't do it must not lose the ability to
            // sync at all.
            //
            // The search is bounded to `low_start..=hi` and its result is
            // filtered to that span here too, whatever the provider
            // promises (review of PR #180, nit 4): RFC 3501's `n:*` names
            // the LAST UID even when it is below n, and re-importing that
            // old message would re-run the on_sync rules on it as "new".
            let sparse_groups: Option<Vec<Vec<i64>>> =
                if low_start <= hi && hi - low_start + 1 > DESCRIPTOR_CHUNK_SIZE {
                    match sync_timing::time("imap.search_uids_in", || {
                        provider.search_uids_in(low_start, hi)
                    }) {
                        Ok(mut uids) => {
                            uids.retain(|uid| (low_start..=hi).contains(uid));
                            uids.sort_unstable();
                            uids.dedup();
                            Some(
                                uids.chunks(DESCRIPTOR_CHUNK_SIZE as usize)
                                    .map(<[i64]>::to_vec)
                                    .collect(),
                            )
                        }
                        Err(_) => None,
                    }
                } else {
                    None
                };

            if let Some(groups) = sparse_groups {
                for group in groups {
                    let Some(&group_high) = group.iter().max() else {
                        continue;
                    };
                    match sync_timing::time("imap.fetch_descriptor_list", || {
                        provider.fetch_descriptor_list(&rmb.path, &group)
                    }) {
                        Ok(chunk) => {
                            if let Some(stop) = apply_descriptor_chunk(
                                db,
                                account_id,
                                &mailbox_id,
                                state.uid_validity,
                                chunk,
                                group_high,
                                &mut acc,
                                &mut new_message_ids,
                            )? {
                                fail_mailbox_mid_chunk_loop(
                                    db,
                                    account_id,
                                    &mailbox_id,
                                    &rmb.path,
                                    cancelled_ops,
                                    &acc,
                                    &descriptor_stop_error(stop),
                                    &mut reports,
                                )?;
                                if provider.session_abandoned() {
                                    break 'mailbox;
                                }
                                continue 'mailbox;
                            }
                        }
                        Err(ProviderError::Unsupported(_)) => {
                            unsupported = true;
                            break;
                        }
                        Err(e) => {
                            fail_mailbox_mid_chunk_loop(
                                db,
                                account_id,
                                &mailbox_id,
                                &rmb.path,
                                cancelled_ops,
                                &acc,
                                &e,
                                &mut reports,
                            )?;
                            if provider.session_abandoned() {
                                break 'mailbox;
                            }
                            continue 'mailbox;
                        }
                    }
                }
                if unsupported {
                    None
                } else {
                    Some(acc)
                }
            } else {
                let mut low = low_start;
                while low <= hi {
                    let high = (low + DESCRIPTOR_CHUNK_SIZE - 1).min(hi);
                    match sync_timing::time("imap.fetch_descriptor_chunk", || {
                        provider.fetch_descriptor_chunk(&rmb.path, low, high)
                    }) {
                        Ok(chunk) => {
                            if let Some(stop) = apply_descriptor_chunk(
                                db,
                                account_id,
                                &mailbox_id,
                                state.uid_validity,
                                chunk,
                                high,
                                &mut acc,
                                &mut new_message_ids,
                            )? {
                                // Partial progress is committed and
                                // checkpointed; the next pass resumes just
                                // past `covered_through`. A clean session
                                // (the reserve reconnect) lets the rest of
                                // this pass continue; an abandoned one ends
                                // it (review of PR #180, findings 1 + 2).
                                fail_mailbox_mid_chunk_loop(
                                    db,
                                    account_id,
                                    &mailbox_id,
                                    &rmb.path,
                                    cancelled_ops,
                                    &acc,
                                    &descriptor_stop_error(stop),
                                    &mut reports,
                                )?;
                                if provider.session_abandoned() {
                                    break 'mailbox;
                                }
                                continue 'mailbox;
                            }
                            low = high + 1;
                        }
                        Err(ProviderError::Unsupported(_)) => {
                            unsupported = true;
                            break;
                        }
                        Err(e) => {
                            fail_mailbox_mid_chunk_loop(
                                db,
                                account_id,
                                &mailbox_id,
                                &rmb.path,
                                cancelled_ops,
                                &acc,
                                &e,
                                &mut reports,
                            )?;
                            if provider.session_abandoned() {
                                break 'mailbox;
                            }
                            continue 'mailbox;
                        }
                    }
                }
                if unsupported {
                    None
                } else {
                    Some(acc)
                }
            }
        } else {
            None
        };

        let (
            fetched,
            descriptors_fetched,
            mut bodies_fetched,
            mut bytes_fetched,
            fetched_uid_min,
            fetched_uid_max,
        ) = if let Some(acc) = chunked_descriptors {
            descriptors_skipped = acc.skipped;
            (acc.fetched, acc.fetched, 0, 0, acc.uid_min, acc.uid_max)
        } else {
            match sync_timing::time("imap.fetch_descriptors", || {
                if skip_fetch {
                    Ok(Vec::new())
                } else {
                    provider.fetch_new_descriptors(after_uid)
                }
            }) {
                Ok(descriptors) => {
                    let fetched = descriptors.len();
                    let fetched_uid_min = descriptors.iter().map(|m| m.uid).min();
                    let fetched_uid_max = descriptors.iter().map(|m| m.uid).max();
                    let (new_messages, keywords) =
                        descriptor_rows(account_id, &mailbox_id, descriptors);
                    new_message_ids.extend(new_messages.iter().map(|nm| nm.id.clone()));
                    // One transaction per DESCRIPTOR_UPSERT_BATCH_SIZE-message
                    // chunk instead of one per message: batches
                    // the commit cost (~0.05ms/tx on this Mac under
                    // WAL/synchronous=FULL, vs ~0.001ms/row inside one) while
                    // keeping the DB mutex hold per call bounded -- measured
                    // p95 ~6-8ms per 200-message batch (see
                    // DESCRIPTOR_UPSERT_BATCH_SIZE's doc comment), so a
                    // concurrent UI read never queues long behind it. Progress
                    // advances once per chunk, not per message.
                    // S42: each batch's non-empty server keyword sets are
                    // written right after it, inside the same timed step.
                    for (chunk, chunk_keywords) in new_messages
                        .chunks(DESCRIPTOR_UPSERT_BATCH_SIZE)
                        .zip(keywords.chunks(DESCRIPTOR_UPSERT_BATCH_SIZE))
                    {
                        sync_timing::time("db.upsert_descriptor", || -> Result<(), DbError> {
                            db.upsert_messages_batch(chunk)?;
                            write_descriptor_keywords(db, chunk, chunk_keywords)
                        })
                        .map_err(db_err)?;
                        sync_progress::advance(account_id, chunk.len() as i64);
                    }
                    (fetched, fetched, 0, 0, fetched_uid_min, fetched_uid_max)
                }
                Err(ProviderError::Unsupported(_)) => {
                    let messages = match provider.fetch_new(after_uid) {
                        Ok(messages) => messages,
                        Err(e) => {
                            let (kind, summary) = redact::redact_provider_error(&e);
                            let _ = db.record_mailbox_sync_failure(
                                account_id,
                                &mailbox_id,
                                kind.as_str(),
                                summary,
                                phase::FETCH,
                            );
                            let total =
                                db.count_messages(account_id, &mailbox_id).map_err(db_err)?;
                            reports.push(MailboxSyncReport {
                                path: rmb.path.clone(),
                                fetched: 0,
                                descriptors_fetched: 0,
                                bodies_fetched: 0,
                                bytes_fetched: 0,
                                fetched_uid_min: None,
                                fetched_uid_max: None,
                                total,
                                cancelled_ops,
                                flags_reconciled: 0,
                                vanished: 0,
                                descriptors_skipped: 0,
                                error: Some(e.to_string()),
                                skipped: None,
                            });
                            continue;
                        }
                    };
                    let fetched = messages.len();
                    let bytes_fetched = messages.iter().map(|m| m.raw.len() as i64).sum();
                    let fetched_uid_min = messages.iter().map(|m| m.uid).min();
                    let fetched_uid_max = messages.iter().map(|m| m.uid).max();
                    for rm in messages {
                        let (nm, extras) =
                            new_message_from_raw(account_id, &mailbox_id, rm, provider_host);
                        db.upsert_message(&nm).map_err(db_err)?;
                        db.set_unsubscribe_targets(
                            &nm.id,
                            extras.list_unsubscribe_mailto.as_deref(),
                            extras.list_unsubscribe_url.as_deref(),
                            extras.list_unsubscribe_one_click,
                        )
                        .map_err(db_err)?;
                        db.upsert_message_trust(&nm.id, &extras.trust)
                            .map_err(db_err)?;
                        persist_calendar_invite(db, &nm.id, &extras)?;
                        persist_message_headers(db, &nm.id, &extras);
                        enqueue_embed_job(db, account_id, &nm.id);
                        new_message_ids.extend([nm.id]);
                        sync_progress::advance(account_id, 1);
                    }
                    (
                        fetched,
                        0,
                        fetched,
                        bytes_fetched,
                        fetched_uid_min,
                        fetched_uid_max,
                    )
                }
                Err(e) => {
                    let (kind, summary) = redact::redact_provider_error(&e);
                    let _ = db.record_mailbox_sync_failure(
                        account_id,
                        &mailbox_id,
                        kind.as_str(),
                        summary,
                        phase::FETCH,
                    );
                    let total = db.count_messages(account_id, &mailbox_id).map_err(db_err)?;
                    reports.push(MailboxSyncReport {
                        path: rmb.path.clone(),
                        fetched: 0,
                        descriptors_fetched: 0,
                        bodies_fetched: 0,
                        bytes_fetched: 0,
                        fetched_uid_min: None,
                        fetched_uid_max: None,
                        total,
                        cancelled_ops,
                        flags_reconciled: 0,
                        vanished: 0,
                        descriptors_skipped: 0,
                        error: Some(e.to_string()),
                        skipped: None,
                    });
                    // A desynchronised session cannot recover: every remaining
                    // mailbox would fail identically against the abandoned guard.
                    // Stop instead of recording N pointless failures.
                    if provider.session_abandoned() {
                        break;
                    }
                    continue;
                }
            }
        };

        // The descriptor (or legacy full-body) import is a durable checkpoint.
        // Commit it before bounded body hydration so a transient body-fetch
        // failure does not re-import descriptors or block later mailboxes.
        //
        // HIGHESTMODSEQ is deliberately NOT advanced here: nothing of the
        // VANISHED/changed delta has been applied yet, so the checkpoint rule
        // (`modseq_to_persist`) says keep the baseline. If the session is
        // abandoned below before reconciliation runs, the baseline is what
        // survives, and the next sync asks the server for that delta again.
        persist_cursor(
            db,
            account_id,
            &mailbox_id,
            &state,
            baseline_modseq,
            &mut modseq_expected,
        )?;

        // Decode-skip retries (review of PR #180, round 2, finding 3): only
        // now, AFTER this mailbox's import is checkpointed, so a retry can
        // never cost the import a reconnect or poison the session under it.
        // A stale row from an old UID space is dropped unconditionally
        // first, defensively — `purge_mailbox_for_uid_space_reset` already
        // does this on a reset, but this covers any row that predates it.
        if !skip_fetch {
            db.delete_stale_message_decode_skips(account_id, &mailbox_id, state.uid_validity)
                .map_err(db_err)?;
            retry_decode_skips(
                db,
                account_id,
                &mailbox_id,
                &rmb.path,
                state.uid_validity,
                provider,
                &mut retry_uids_left,
                // Recovered decode skips are old mail, not arrivals: they
                // join `all` only (see `NewArrivals`).
                &mut new_message_ids.all,
            )?;
            // Bug hunt #69: bring back messages whose archive/delete/Empty Trash
            // was cancelled (rows recorded by `Database::cancel_operation`).
            refetch_cancelled_removals(
                db,
                account_id,
                &mailbox_id,
                &rmb.path,
                state.uid_validity,
                provider,
                &mut new_message_ids.all,
            )?;
            // The retry budget always reconnects after a decode failure, so
            // this only trips if that reconnect itself failed or the
            // connection dropped mid-retry (`Interrupted` + closed).
            if provider.session_abandoned() {
                let (kind, summary) = redact::redact_provider_error(&descriptor_stop_error(
                    DescriptorFetchIncomplete {
                        covered_through: None,
                        reason: DescriptorFetchStop::SessionAbandoned,
                    },
                ));
                let _ = db.record_mailbox_sync_failure(
                    account_id,
                    &mailbox_id,
                    kind.as_str(),
                    summary,
                    phase::FETCH,
                );
                let total = db.count_messages(account_id, &mailbox_id).map_err(db_err)?;
                reports.push(MailboxSyncReport {
                    path: rmb.path.clone(),
                    fetched,
                    descriptors_fetched,
                    bodies_fetched,
                    bytes_fetched,
                    fetched_uid_min,
                    fetched_uid_max,
                    total,
                    cancelled_ops,
                    flags_reconciled: 0,
                    vanished: 0,
                    descriptors_skipped,
                    error: Some(DescriptorFetchStop::SessionAbandoned.as_str().to_string()),
                    skipped: None,
                });
                break 'mailbox;
            }
        }

        let missing_body_uids = db
            .list_body_missing_uids(account_id, &mailbox_id, BODY_HYDRATION_BATCH_LIMIT)
            .map_err(db_err)?;
        // A body-fetch failure on a live session: reported at the end of
        // this mailbox, after reconciliation has still run.
        let mut hydrate_error: Option<String> = None;
        if !missing_body_uids.is_empty() {
            match sync_timing::time("imap.fetch_bodies", || {
                provider.fetch_bodies(&missing_body_uids)
            }) {
                Ok(messages) => {
                    bodies_fetched += messages.len();
                    bytes_fetched += messages.iter().map(|m| m.raw.len() as i64).sum::<i64>();
                    for rm in messages {
                        let (nm, extras) = sync_timing::time("core.parse_body", || {
                            new_message_from_raw(account_id, &mailbox_id, rm, provider_host)
                        });
                        sync_timing::time("db.persist_body", || -> Result<(), CoreError> {
                            db.upsert_message(&nm).map_err(db_err)?;
                            db.set_unsubscribe_targets(
                                &nm.id,
                                extras.list_unsubscribe_mailto.as_deref(),
                                extras.list_unsubscribe_url.as_deref(),
                                extras.list_unsubscribe_one_click,
                            )
                            .map_err(db_err)?;
                            db.upsert_message_trust(&nm.id, &extras.trust)
                                .map_err(db_err)?;
                            persist_calendar_invite(db, &nm.id, &extras)?;
                            persist_message_headers(db, &nm.id, &extras);
                            enqueue_embed_job(db, account_id, &nm.id);
                            Ok(())
                        })?;
                    }
                }
                Err(ProviderError::Unsupported(_)) => {}
                Err(e) => {
                    let (kind, summary) = redact::redact_provider_error(&e);
                    let _ = db.record_mailbox_sync_failure(
                        account_id,
                        &mailbox_id,
                        kind.as_str(),
                        summary,
                        phase::HYDRATE,
                    );
                    // A session the failure poisoned (an undecodable
                    // response) cannot carry the flag scan either, and no
                    // later mailbox can use it: report and stop the pass,
                    // exactly as the decode-skip retry path above does.
                    if provider.session_abandoned() {
                        let total = db.count_messages(account_id, &mailbox_id).map_err(db_err)?;
                        reports.push(MailboxSyncReport {
                            path: rmb.path.clone(),
                            fetched,
                            descriptors_fetched,
                            bodies_fetched,
                            bytes_fetched,
                            fetched_uid_min,
                            fetched_uid_max,
                            total,
                            cancelled_ops,
                            flags_reconciled: 0,
                            vanished: 0,
                            descriptors_skipped,
                            error: Some(e.to_string()),
                            skipped: None,
                        });
                        break 'mailbox;
                    }
                    // Otherwise the failure is NOT fatal to reconciliation
                    // (bug hunt 2026-10-12, #3456). This used to `continue`,
                    // and because the same newest body-missing UIDs are
                    // picked again every pass, one message the server always
                    // refuses to serve meant the folder's expunges and flag
                    // changes were never applied again. Bodies and the
                    // flag/vanish delta are independent: the missing-body
                    // queue is the hydration checkpoint, HIGHESTMODSEQ only
                    // covers the delta. So fall through, and keep the error
                    // for the report and the diagnostic row below.
                    hydrate_error = Some(e.to_string());
                }
            }
        }

        // Three reconciliation strategies: apply the QRESYNC delta this
        // mailbox's SELECT already carried (cheap — no extra round trip);
        // skip reconciliation entirely after a from-scratch fetch (the tracker
        // #2502); or fall back to the full RFC 4549-style UID+FLAGS scan
        // every prior build has always used. All three record through
        // `DeltaOutcome`, so all three honour the same op locks (a message
        // with a queued per-message op, or in a mailbox with a queued
        // mark_all_read, is never purged or silently flag-flipped out from
        // under the replay queue) and all three report whether anything was
        // held back.
        let outcome = if let Some(delta) = &qresync_outcome {
            locks.refresh(db, account_id)?;
            sync_timing::time("imap.apply_qresync_delta", || {
                apply_qresync_delta(db, account_id, &mailbox_id, delta, &locks)
            })?
        } else if !same_uid_space {
            // `!same_uid_space` is exactly the condition (checked above) that
            // just purged every local row for this mailbox and refetched
            // from scratch — no cursor yet, or a UIDVALIDITY reset (the tracker
            // #2619's "cursor None but local rows exist" folds into this
            // same purge-and-refetch path, so it's covered too). The local
            // set now equals exactly what `fetch_new_descriptors`/`fetch_new`
            // just imported, WITH the flags that fetch itself carried (`UID
            // FETCH ... FLAGS ...` / raw FLAGS on the legacy path) — nothing
            // is held back, there is nothing to reconcile. A full `UID FETCH
            // 1:* (UID FLAGS)` right after would only ask the server to
            // reconfirm what it told us seconds earlier: measured +18s/+28%
            // on a real 78k-message Gmail INBOX first sync (see
            // project-docs/perf/2026-09-sync-baseline.md).
            DeltaOutcome::new()
        } else {
            match sync_timing::time("imap.list_uid_flags", || provider.list_uid_flags()) {
                Ok(remote) => {
                    // After the snapshot returned, as close as possible to
                    // the local read inside `reconcile_full_scan`.
                    locks.refresh(db, account_id)?;
                    let reconcile_start = sync_timing::now();
                    let outcome =
                        reconcile_full_scan(db, account_id, &mailbox_id, &remote, &locks)?;
                    sync_timing::record_since("db.reconcile_flags", reconcile_start);
                    outcome
                }
                // Any provider error means "reconciliation unavailable": skip
                // it and NEVER purge, so a transient failure can't wipe the
                // cache — and nothing was applied, so nothing may advance.
                Err(_) => {
                    let mut outcome = DeltaOutcome::new();
                    outcome.hold_back();
                    outcome
                }
            }
        };
        let new_highest_modseq =
            modseq_to_persist(baseline_modseq, reported_modseq, outcome.applied_all);

        // Reconciliation has now run (or was safely skipped): persist the
        // HIGHESTMODSEQ the checkpoint rule resolved above.
        persist_cursor(
            db,
            account_id,
            &mailbox_id,
            &state,
            new_highest_modseq,
            &mut modseq_expected,
        )?;

        // A hydration failure stays on record: the HYDRATE diagnostic row
        // written above must not be overwritten by a success, and the report
        // carries the error next to the real reconciliation counts.
        if hydrate_error.is_none() {
            let _ = db.record_mailbox_sync_success(account_id, &mailbox_id);
        }
        let total = db.count_messages(account_id, &mailbox_id).map_err(db_err)?;
        reports.push(MailboxSyncReport {
            path: rmb.path.clone(),
            fetched,
            descriptors_fetched,
            bodies_fetched,
            bytes_fetched,
            fetched_uid_min,
            fetched_uid_max,
            total,
            cancelled_ops,
            flags_reconciled: outcome.flags_reconciled,
            vanished: outcome.vanished,
            descriptors_skipped,
            error: hydrate_error,
            skipped: None,
        });
    }

    // Folders the server no longer lists (deleted or renamed elsewhere) are
    // removed here, after the loop so a session abandoned mid-pass is known,
    // and before the thread rebuild so the purge's dirty marker is honoured
    // in this same pass. Every safeguard is in the function's doc comment.
    let removed_mailboxes =
        remove_mailboxes_gone_from_server(db, account_id, &remote, provider.session_abandoned())?;

    // S36: the Drafts mailbox (already synced by default — see
    // should_sync_by_default) may now disagree with a locally-tracked
    // server draft (another client replaced it). Reconcile once per
    // account, per sync — read-only w.r.t. the server, so failure here must
    // never fail the whole sync; there is no logging facility in this crate,
    // so a failure here is simply invisible rather than surfaced, an
    // acceptable degradation next to losing sync entirely.
    let _ = db.reconcile_server_draft_conflicts(account_id);

    // Rebuild the thread projection across all of the account's mailboxes --
    // but only when something actually needs it. `rebuild_threads` scans
    // every message + reference row for the WHOLE account (not just the
    // mailboxes just synced), so on a large account it dominates the pass.
    // The real invariant (S64 review fix, corrected again after the review
    // found `upsert_message`/`delete_messages_for_mailbox`/
    // `clear_sync_cursor` could change thread membership or aggregates with
    // neither flag set): the DB layer marks the account dirty, in the SAME
    // transaction, on EVERY write that can change thread membership or a
    // thread aggregate -- `mark_read`/`mark_all_read`/delete-move/
    // empty_trash for local mutations, and `upsert_message` (when hydration
    // replaces an existing row's References chain), `delete_messages_for_
    // mailbox` and `clear_sync_cursor` for purges -- via `mark_threads_dirty`
    // (migration 0037's `thread_rebuild_state`; `rebuild_threads` clears it).
    // Sync additionally rebuilds whenever THIS pass itself fetched, reconciled
    // or vanished something remote. Skipping when neither is true relies on
    // that DB-layer invariant holding for every current and future
    // aggregate-changing write, not on sync.rs's own reasoning about which
    // writes matter. Found by the S64 read-path gate: a QRESYNC no-op resync
    // (S30) cost as much db_core time as the pre-S30 full-scan no-op,
    // because both always paid this full rebuild regardless of the
    // (correctly empty) reconciliation outcome.
    //: `rebuild_threads` itself is now incremental --
    // `rebuild_threads_incremental` recomputes only the threads
    // `thread_dirty_messages` names (every write path above that can touch
    // one records the affected message id(s), including a brand-new
    // insert), falling back to a full rebuild only for a first run, a
    // force_full purge/UIDVALIDITY reset, or a dirty set too large to
    // bother walking incrementally. The `any_remote_change` OR `threads_
    // dirty` decision below is UNCHANGED from the S64 review fix: every
    // remote fetch/reconcile/vanish already marks its own message(s) dirty
    // at the DB layer now, so this call is really just "is there anything
    // in thread_dirty_messages worth looking at", kept as its own check so
    // a mailbox that reported nothing new never even takes the lock to ask.
    let any_remote_change = reports
        .iter()
        .any(|r| r.fetched > 0 || r.flags_reconciled > 0 || r.vanished > 0);
    if any_remote_change || db.threads_dirty(account_id).map_err(db_err)? {
        sync_timing::time("db.rebuild_threads", || {
            db.rebuild_threads_incremental(account_id)
        })
        .map_err(db_err)?;
    }

    // S42: new arrivals in a muted conversation are marked read (local
    // always; \Seen STORE only when writes are enabled — mark_read_auto's
    // own policy). Best-effort, like the rules pass below: never fails the
    // sync that just landed them.
    if !new_message_ids.all.is_empty() {
        let _ = sync_timing::time("core.mute_on_arrival", || -> Result<(), DbError> {
            for id in db.muted_among(&new_message_ids.all)? {
                db.mark_read_auto(&id, true)?;
            }
            Ok(())
        });
    }

    // S40: run every on_sync rule for this account against exactly the
    // mail that just ARRIVED in THIS pass (`NewArrivals::for_rules`: an
    // Inbox's incremental new mail, never a first import, a reset or
    // another folder — MM-3 review) — never a full mailbox rescan
    // (Assumption 3) — and only when something new actually arrived (a
    // no-op sync pass must not do a pointless rules pass). Best-effort, like
    // `enqueue_embed_job` above: a rules-pass failure must never fail the
    // sync that just landed the mail it would have evaluated. (Not recorded
    // via `record_mailbox_sync_failure` — that table's `mailbox_id` has a
    // real FK to `mailboxes`, and a rules pass runs across every mailbox in
    // the account at once, so there is no single mailbox to attribute the
    // failure to.)
    let _ = sync_timing::time("core.rules", || {
        crate::rules::run_rules_deferring_unhydrated(db, account_id, &new_message_ids.for_rules)
    });

    // AW2: resolve the evidence of waiting-for-reply proposals whose Sent
    // copy just landed, and propose "needs reply" for genuine inbox arrivals
    // in a thread with a waiting work item. Proposals only (the user accepts),
    // after the thread rebuild so thread ids are current; best-effort.
    let _ = sync_timing::time("core.suggestions", || {
        crate::suggestions::after_sync_arrivals_now(
            db,
            &new_message_ids.all,
            &new_message_ids.for_rules,
        )
    });

    // S59: new-mail notification candidates, AFTER mute-on-arrival and the
    // rules pass so their effects (marked read, muted) are already in the DB.
    // Message ids are `{mailbox_id}:{uid}`; only warm mailboxes qualify.
    // `enqueue_notifications` applies the inbox-role, unread, not-muted,
    // not-snoozed and 24 h predicates. Best-effort: never fails the sync.
    let notify_candidate_ids: Vec<String> = new_message_ids
        .all
        .iter()
        .filter(|id| {
            id.rsplit_once(':')
                .is_some_and(|(mb, _)| warm_mailboxes.contains(mb))
        })
        .cloned()
        .collect();
    if !notify_candidate_ids.is_empty() {
        let _ = sync_timing::time("core.notify_enqueue", || {
            db.enqueue_notifications(account_id, &notify_candidate_ids, now_epoch())
        });
    }

    sync_progress::finish(account_id);
    Ok(SyncReport {
        account_id: account_id.to_string(),
        mailboxes: reports,
        removed_mailboxes: removed_mailboxes.paths,
        removed_mailbox_cancelled_ops: removed_mailboxes.cancelled_ops,
        orphaned_attachment_hashes: removed_mailboxes.orphaned_hashes,
    })
}

/// What `new_message_from_raw` resolves alongside the `NewMessage` row:
/// values parsed at ingest that live in their own tables rather than on
/// `messages` itself.
///
/// A struct rather than a tuple because S44 and S48 both extended this return
/// value independently, and the merge of the two would otherwise have been
/// `(NewMessage, Option<String>, Option<String>, bool, TrustSignals)` — two
/// same-typed Options and a positional bool, at eleven call sites. Named
/// fields also mean the next stage that parses something at ingest adds a
/// field instead of changing the arity everywhere.
pub(crate) struct IngestExtras {
    /// RFC 2369 `mailto:` unsubscribe target, if any (S44).
    pub list_unsubscribe_mailto: Option<String>,
    /// RFC 2369 `https:` unsubscribe target, if any (S44).
    pub list_unsubscribe_url: Option<String>,
    /// Whether the message opts into RFC 8058 one-click (S44).
    pub list_unsubscribe_one_click: bool,
    /// SPF/DKIM/DMARC signals from Authentication-Results (S48).
    pub trust: castellum_models::TrustSignals,
    /// The message's parsed calendar invite (S55), if it carries one.
    /// `our_response`/`responded_at` are always `None` here — this is the
    /// freshly-parsed view, not the durable one; persisting it (see
    /// `persist_calendar_invite`) never overwrites a response already on
    /// record (`Database::upsert_calendar_invite`'s `ON CONFLICT` clause).
    pub calendar_invite: Option<castellum_models::CalendarInvite>,
    /// Headers kept for any-header smart-mailbox conditions (MM-1c, #2710):
    /// policy-filtered, capped, decoded. Empty when extraction found nothing
    /// or failed (a capture row is still written, see `persist_message_headers`).
    pub headers: Vec<castellum_models::StoredHeader>,
    /// The extractor dropped something (cap hit, parse failure, panic): store
    /// the capture as partial, i.e. unknown for "absent" and negated conditions.
    pub headers_truncated: bool,
}

/// Replace `message_id`'s stored headers from `extras.headers` and mark the
/// message captured (MM-1c). Called next to `persist_calendar_invite` at every
/// `new_message_from_raw` call site with a full body. Never fails the caller:
/// header capture is an enhancement, so a DB error here is logged and
/// otherwise dropped (the message simply stays "unknown" and is retried the
/// next time its body is hydrated). The log line names the message id only:
/// header names and values are mail content and never logged.
pub(crate) fn persist_message_headers(db: &Database, message_id: &str, extras: &IngestExtras) {
    if let Err(e) = db.replace_message_headers(
        message_id,
        &extras.headers,
        castellum_models::HeaderCaptureSource::Body,
        extras.headers_truncated,
    ) {
        // `DbError` text comes from SQLite / this crate's own messages, never
        // from a bound header value.
        tracing::warn!(message_id, "storing message headers failed: {e}");
    }
}

/// Persist `extras.calendar_invite` for `message_id`, if present. Called
/// right after `upsert_message_trust` at every `new_message_from_raw` call
/// site — pulled out into one helper so the 4 sites can't drift on whether
/// they remember to guard the `None` case.
pub(crate) fn persist_calendar_invite(
    db: &Database,
    message_id: &str,
    extras: &IngestExtras,
) -> Result<(), CoreError> {
    let Some(invite) = &extras.calendar_invite else {
        return Ok(());
    };
    db.upsert_calendar_invite(message_id, invite)
        .map_err(db_err)
}

/// Builds the `NewMessage` upsert row from a fully-fetched remote message,
/// alongside the 3 List-Unsubscribe fields `parse_message` resolved (S44) —
/// NOT stored on `NewMessage` itself (see the plan's Assumptions §5: no
/// `Default` impl, 34 literal-construction call sites). Every caller that has
/// a `message_id` before calling this should persist them via
/// `Database::set_unsubscribe_targets` right after its own `upsert_message`.
pub(crate) fn new_message_from_raw(
    account_id: &str,
    mailbox_id: &str,
    rm: RemoteMessage,
    provider_host: &str,
) -> (NewMessage, IngestExtras) {
    let parsed = parse_message(&rm.raw);
    // Wall 1: sanitize once at ingest. A sanitizer panic must never fail sync —
    // fall back to text-only rendering for that message.
    let sanitized = parsed.html_body.as_deref().and_then(|h| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            castellum_mail::sanitize_email_html(h)
        }))
        .ok()
    });
    let (sanitized_html, has_remote_content) = match sanitized {
        Some(s) => (Some(s.html), s.has_remote_content),
        None => (None, false),
    };
    // A malformed/hostile header must never crash sync (Task 1's mandatory
    // "no false pass" test covers the correctness half; this guards process
    // survival the same way the sanitizer above does).
    let trust = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        castellum_mail::extract_trust_signals(&rm.raw, provider_host)
    }))
    .unwrap_or_default();
    // Same containment for header extraction (untrusted, RFC 2047-decoded).
    // A panic is a failed extraction: an empty, TRUNCATED (partial) capture.
    let (headers, headers_truncated) =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            castellum_mail::extract_stored_headers(&rm.raw)
        }))
        .unwrap_or((Vec::new(), true));
    let nm = NewMessage {
        id: format!("{mailbox_id}:{}", rm.uid),
        account_id: account_id.to_string(),
        mailbox_id: mailbox_id.to_string(),
        provider_uid: Some(rm.uid),
        rfc822_message_id: parsed.rfc822_message_id,
        subject: parsed.subject,
        from_name: parsed.from_name,
        from_address: parsed.from_address,
        sent_at: parsed.sent_at,
        snippet: parsed.snippet,
        size_bytes: Some(rm.raw.len() as i64),
        has_attachments: parsed.has_attachments,
        attachments: Some(parsed.attachments),
        is_read: rm.is_seen,
        is_starred: rm.is_flagged,
        addresses: parsed.addresses,
        text_body: parsed.text_body,
        html_body: parsed.html_body,
        sanitized_html,
        has_remote_content,
        references: parsed.references,
    };
    (
        nm,
        IngestExtras {
            list_unsubscribe_mailto: parsed.list_unsubscribe_mailto,
            list_unsubscribe_url: parsed.list_unsubscribe_url,
            list_unsubscribe_one_click: parsed.list_unsubscribe_one_click,
            trust,
            calendar_invite: parsed.calendar_invite,
            headers,
            headers_truncated,
        },
    )
}

fn new_message_from_descriptor(
    account_id: &str,
    mailbox_id: &str,
    descriptor: RemoteMessageDescriptor,
) -> NewMessage {
    let mut addresses = Vec::new();
    if let Some(from) = descriptor.from.clone() {
        addresses.push(remote_address_to_model("from", from));
    }
    addresses.extend(
        descriptor
            .to
            .into_iter()
            .map(|a| remote_address_to_model("to", a)),
    );
    addresses.extend(
        descriptor
            .cc
            .into_iter()
            .map(|a| remote_address_to_model("cc", a)),
    );
    addresses.extend(
        descriptor
            .bcc
            .into_iter()
            .map(|a| remote_address_to_model("bcc", a)),
    );
    NewMessage {
        id: format!("{mailbox_id}:{}", descriptor.uid),
        account_id: account_id.to_string(),
        mailbox_id: mailbox_id.to_string(),
        provider_uid: Some(descriptor.uid),
        rfc822_message_id: descriptor.rfc822_message_id,
        subject: descriptor.subject,
        from_name: descriptor.from.as_ref().and_then(|a| a.name.clone()),
        from_address: descriptor.from.as_ref().map(|a| a.address.clone()),
        sent_at: descriptor.sent_at,
        snippet: None,
        size_bytes: descriptor.size_bytes,
        has_attachments: descriptor.has_attachments.unwrap_or(false),
        attachments: None,
        is_read: descriptor.is_seen,
        is_starred: descriptor.is_flagged,
        addresses,
        text_body: None,
        html_body: None,
        sanitized_html: None,
        has_remote_content: false,
        references: descriptor.references,
    }
}

fn remote_address_to_model(kind: &str, address: RemoteAddress) -> Address {
    Address {
        kind: kind.to_string(),
        name: address.name,
        address: address.address,
    }
}

pub(crate) fn should_sync_by_default(mailbox: &RemoteMailbox, role: Option<&str>) -> bool {
    if !mailbox.selectable {
        return false;
    }
    if mailbox.path.eq_ignore_ascii_case("INBOX") {
        return true;
    }
    if mailbox.subscribed {
        return true;
    }
    match role {
        Some("inbox" | "sent" | "drafts" | "trash" | "junk") => true,
        // Archive / All Mail can be very large. It is discovered and selectable,
        // but waits for explicit user selection or a subscribed signal.
        Some("archive") => false,
        Some(_) | None => false,
    }
}

pub(crate) fn effective_role(
    mailbox: &RemoteMailbox,
    canonical_roles: &HashMap<String, String>,
) -> Option<String> {
    let role = mailbox.role.as_ref()?;
    match canonical_roles.get(role) {
        Some(path) if path == &mailbox.path => Some(role.clone()),
        _ => None,
    }
}

pub(crate) fn canonical_role_paths(mailboxes: &[RemoteMailbox]) -> HashMap<String, String> {
    let mut out: HashMap<String, (RoleCandidateRank, String)> = HashMap::new();
    for mailbox in mailboxes {
        let Some(role) = mailbox.role.as_ref() else {
            continue;
        };
        let rank = role_candidate_rank(mailbox, role);
        match out.get(role) {
            Some((best_rank, _)) if best_rank <= &rank => {}
            _ => {
                out.insert(role.clone(), (rank, mailbox.path.clone()));
            }
        }
    }
    out.into_iter()
        .map(|(role, (_, path))| (role, path))
        .collect()
}

type RoleCandidateRank = (u8, u8, u8, u8, String);

fn role_candidate_rank(mailbox: &RemoteMailbox, role: &str) -> RoleCandidateRank {
    let selectable = if mailbox.selectable { 0 } else { 1 };
    let provenance = match mailbox.role_source {
        Some(MailboxRoleSource::SpecialUse) => 0,
        Some(MailboxRoleSource::Heuristic) => 1,
        None => 2,
    };
    let preferred_name = if is_preferred_role_name(mailbox, role) {
        0
    } else {
        1
    };
    let subscribed = if mailbox.subscribed { 0 } else { 1 };
    (
        selectable,
        provenance,
        preferred_name,
        subscribed,
        mailbox.path.to_ascii_lowercase(),
    )
}

fn is_preferred_role_name(mailbox: &RemoteMailbox, role: &str) -> bool {
    let leaf = mailbox.name.to_ascii_lowercase();
    match role {
        "inbox" => mailbox.path.eq_ignore_ascii_case("INBOX"),
        "sent" => matches!(leaf.as_str(), "sent" | "sent messages" | "sent mail"),
        "drafts" => leaf == "drafts",
        "trash" => matches!(leaf.as_str(), "trash" | "deleted messages"),
        "junk" => matches!(leaf.as_str(), "junk" | "spam"),
        "archive" => leaf == "archive",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use castellum_models::TrustResult;
    use castellum_providers::{
        MailboxState, QresyncSync, RemoteAddress, RemoteMailbox, RemoteMessage,
        RemoteMessageDescriptor, UidFlags, UidRangeSet,
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn mark_all_op(payload: serde_json::Value) -> OperationRecord {
        serde_json::from_value(serde_json::json!({
            "id": 1, "account_id": "acc", "kind": "mark_all_read",
            "payload_json": payload.to_string(), "status": "queued",
            "attempts": 0, "last_error": null, "updated_at_epoch": 0,
        }))
        .unwrap()
    }

    #[test]
    fn queued_mark_all_read_locks_only_its_snapshot_uids() {
        // FP27: a message that arrived after Mark All as Read was pressed is
        // not held as read by the pending op.
        let op = mark_all_op(
            serde_json::json!({"mailbox_id": "acc:INBOX", "uid_validity": 9, "uids": "1:5,8"}),
        );
        let locks = OpLocks::from_pending(&[op]);
        assert!(locks.read_lock("acc:INBOX", "acc:INBOX:1"));
        assert!(locks.read_lock("acc:INBOX", "acc:INBOX:5"));
        assert!(locks.read_lock("acc:INBOX", "acc:INBOX:8"));
        assert!(!locks.read_lock("acc:INBOX", "acc:INBOX:6"));
        assert!(!locks.read_lock("acc:INBOX2", "acc:INBOX2:1"));
        // A legacy op (no snapshot) still locks the whole mailbox.
        let legacy = mark_all_op(serde_json::json!({"mailbox_id": "acc:INBOX"}));
        let locks = OpLocks::from_pending(&[legacy]);
        assert!(locks.read_lock("acc:INBOX", "acc:INBOX:6"));
    }

    #[test]
    fn failed_permanent_state_ops_do_not_lock_the_message() {
        // Audit #3451: a dead mark_read/flag/keyword op froze the flag and pinned
        // the message against server deletions on every later sync.
        let mk = |kind: &str, status: &str| -> OperationRecord {
            serde_json::from_value(serde_json::json!({
                "id": 1, "account_id": "acc", "kind": kind,
                "payload_json": serde_json::json!({"message_id": "acc:INBOX:1"}).to_string(),
                "status": status, "attempts": 8, "last_error": "NO", "updated_at_epoch": 0,
            }))
            .unwrap()
        };
        let live = OpLocks::from_pending(&[
            mk("mark_read", "failed"),
            mk("flag", "queued"),
            mk("keyword", "running"),
        ]);
        assert!(live.read_lock("acc:INBOX", "acc:INBOX:1"));
        assert!(live.star_lock("acc:INBOX:1"));
        assert!(live.keyword_lock("acc:INBOX:1"));
        let dead = OpLocks::from_pending(&[
            mk("mark_read", "failed_permanent"),
            mk("flag", "failed_permanent"),
            mk("keyword", "failed_permanent"),
        ]);
        assert!(!dead.read_lock("acc:INBOX", "acc:INBOX:1"));
        assert!(!dead.star_lock("acc:INBOX:1"));
        assert!(!dead.keyword_lock("acc:INBOX:1"));
    }

    fn temp_db() -> Database {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-sync-test-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        Database::open(&path).expect("open db")
    }

    fn raw_email(uid: i64) -> Vec<u8> {
        format!(
            "From: Alice <alice@example.com>\r\n\
             To: test@local.test\r\n\
             Subject: Message {uid}\r\n\
             Message-ID: <{uid}@example.com>\r\n\
             Date: Tue, 1 Jul 2025 10:00:00 +0000\r\n\
             \r\n\
             Body of message {uid}.\r\n"
        )
        .into_bytes()
    }

    /// A scriptable in-memory provider for deterministic sync tests.
    struct MockProvider {
        state: MailboxState,
        messages: Vec<RemoteMessage>,
    }

    impl MailProvider for MockProvider {
        fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
            Ok(vec![RemoteMailbox {
                path: "INBOX".to_string(),
                name: "INBOX".to_string(),
                role: Some("inbox".to_string()),
                role_source: Some(MailboxRoleSource::SpecialUse),
                delimiter: Some("/".to_string()),
                subscribed: true,
                selectable: true,
            }])
        }
        fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
            Ok(self.state)
        }
        fn fetch_new(
            &mut self,
            after_uid: Option<i64>,
        ) -> Result<Vec<RemoteMessage>, ProviderError> {
            Ok(self
                .messages
                .iter()
                .filter(|m| after_uid.is_none_or(|a| m.uid > a))
                .cloned()
                .collect())
        }
        fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
            // The mock's `messages` vec IS the current server state: mutate a
            // message's flags to model a remote flag change, or drop it to model
            // an expunge/move.
            Ok(self
                .messages
                .iter()
                .map(|m| UidFlags {
                    keywords: None,
                    uid: m.uid,
                    is_seen: m.is_seen,
                    is_flagged: m.is_flagged,
                })
                .collect())
        }
        fn logout(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    /// Wraps any `MailProvider` and counts `list_uid_flags` calls, without
    /// touching any of the 20+ existing `MockProvider { .. }` construction
    /// sites. Used only by the regression test: a from-scratch
    /// descriptor fetch must never trigger the redundant full UID+FLAGS scan.
    struct CountingProvider<P> {
        inner: P,
        list_uid_flags_calls: usize,
    }

    impl<P: MailProvider> MailProvider for CountingProvider<P> {
        fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
            self.inner.list_mailboxes()
        }
        fn select(&mut self, path: &str) -> Result<MailboxState, ProviderError> {
            self.inner.select(path)
        }
        fn fetch_new(
            &mut self,
            after_uid: Option<i64>,
        ) -> Result<Vec<RemoteMessage>, ProviderError> {
            self.inner.fetch_new(after_uid)
        }
        fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
            self.list_uid_flags_calls += 1;
            self.inner.list_uid_flags()
        }
        fn logout(&mut self) -> Result<(), ProviderError> {
            self.inner.logout()
        }
    }

    /// Wraps any `MailProvider` and reports its session abandoned from the
    /// very first check, with a fixed `SessionAbandonReason`. the tracker
    /// #2638's account-level classification tests need to control the
    /// REASON directly, not just whether the session is abandoned (which
    /// `ChunkedMock`'s `stop_on_call` already covers, always as the
    /// decode-failure-shaped default).
    struct AbandonedWithReason<P> {
        inner: P,
        reason: castellum_providers::SessionAbandonReason,
    }

    impl<P: MailProvider> MailProvider for AbandonedWithReason<P> {
        fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
            self.inner.list_mailboxes()
        }
        fn select(&mut self, path: &str) -> Result<MailboxState, ProviderError> {
            self.inner.select(path)
        }
        fn fetch_new(
            &mut self,
            after_uid: Option<i64>,
        ) -> Result<Vec<RemoteMessage>, ProviderError> {
            self.inner.fetch_new(after_uid)
        }
        fn session_abandoned(&self) -> bool {
            true
        }
        fn session_abandon_reason(&self) -> Option<castellum_providers::SessionAbandonReason> {
            Some(self.reason)
        }
        fn logout(&mut self) -> Result<(), ProviderError> {
            self.inner.logout()
        }
    }

    fn msg(uid: i64) -> RemoteMessage {
        RemoteMessage {
            uid,
            raw: raw_email(uid),
            is_seen: false,
            is_flagged: false,
        }
    }

    fn rmb(
        path: &str,
        name: &str,
        role: Option<&str>,
        delimiter: Option<&str>,
        subscribed: bool,
        selectable: bool,
    ) -> RemoteMailbox {
        RemoteMailbox {
            path: path.into(),
            name: name.into(),
            role: role.map(str::to_string),
            role_source: role.map(|_| MailboxRoleSource::Heuristic),
            delimiter: delimiter.map(str::to_string),
            subscribed,
            selectable,
        }
    }

    fn rmb_special(
        path: &str,
        name: &str,
        role: Option<&str>,
        delimiter: Option<&str>,
        subscribed: bool,
        selectable: bool,
    ) -> RemoteMailbox {
        let mut mailbox = rmb(path, name, role, delimiter, subscribed, selectable);
        mailbox.role_source = role.map(|_| MailboxRoleSource::SpecialUse);
        mailbox
    }

    /// A `origin='local'` keyword row with no queued op: written while the
    /// account is in evaluation mode, then writes are switched back on.
    fn seed_local_keyword(db: &Database, message_id: &str, keyword: &str) {
        db.set_account_write_enabled("acc", false).unwrap();
        db.set_message_keyword(message_id, keyword, true).unwrap();
        db.set_account_write_enabled("acc", true).unwrap();
        assert!(db.pending_operations("acc").unwrap().is_empty());
    }

    fn seed_account_mailbox(db: &Database) {
        db.upsert_account(&Account {
            id: "acc".into(),
            email: "test@local.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        // Reconciliation tests queue local flag/mark-all intent; needs writes.
        db.set_account_write_enabled("acc", true).unwrap();
        db.upsert_mailbox(&Mailbox {
            id: "acc:INBOX".into(),
            account_id: "acc".into(),
            path: "INBOX".into(),
            name: "INBOX".into(),
            role: Some("inbox".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
    }

    fn seed_inbox_cursor(db: &Database, uid_validity: i64, uid_next: i64) {
        db.set_sync_cursor(
            "acc",
            "acc:INBOX",
            &SyncCursor {
                uid_validity: Some(uid_validity),
                uid_next: Some(uid_next),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
    }

    fn seed_inbox_cursor_with_modseq(db: &Database, uid_validity: i64, uid_next: i64, modseq: i64) {
        db.set_sync_cursor(
            "acc",
            "acc:INBOX",
            &SyncCursor {
                uid_validity: Some(uid_validity),
                uid_next: Some(uid_next),
                highest_modseq: Some(modseq),
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
    }

    /// A scriptable provider for QRESYNC-path sync tests (S30) — deliberately
    /// separate from `MockProvider` (which has no QRESYNC surface at all),
    /// matching this file's own established one-mock-per-scenario convention.
    struct QresyncMock {
        qresync_available: bool,
        state: MailboxState,
        selects: usize,
        select_qresyncs: usize,
        /// What `select_qresync` returns on every call; `None` makes it
        /// return `Unsupported` (simulating a stale/never-eligible cursor
        /// still reaching the call, which `sync_with_provider` must never
        /// do, but the mock stays honest about the trait's contract either
        /// way).
        outcome: Option<QresyncSync>,
        /// When true, `fetch_bodies` returns a transient network error
        /// instead of its default `Ok(vec![])` — simulates a body-fetch
        /// failure landing after the QRESYNC delta was already fetched.
        fail_body_fetch: bool,
        /// What `last_highest_modseq()` reports — models the HIGHESTMODSEQ a
        /// plain `select()` captures once CONDSTORE is enabled (the
        /// post-upgrade path: a cursor with no modseq yet on a QRESYNC server).
        last_modseq: Option<i64>,
        /// The `list_uid_flags()` snapshot for the RFC 4549 full-scan path;
        /// `None` makes it return `Unsupported` (the trait default).
        snapshot: Option<Vec<UidFlags>>,
        /// The `highest_modseq` argument of every `select_qresync` call, in
        /// order — proves which baseline the next delta was requested from.
        qresync_modseq_args: Vec<i64>,
        /// With `fail_body_fetch`: the failure also poisons the session
        /// (`session_abandoned()` turns true), like an undecodable response.
        abandon_on_body_fetch: bool,
        abandoned: bool,
        /// What `fetch_descriptor_list` can serve: messages the server still
        /// holds after a cancelled archive (bug hunt #69). `None` = Unsupported.
        listable: Option<Vec<RemoteMessageDescriptor>>,
        /// Every UID list `fetch_descriptor_list` was asked for.
        list_calls: Vec<Vec<i64>>,
    }

    impl Default for QresyncMock {
        fn default() -> Self {
            QresyncMock {
                qresync_available: false,
                state: MailboxState {
                    uid_validity: 0,
                    uid_next: 0,
                    exists: 0,
                },
                selects: 0,
                select_qresyncs: 0,
                outcome: None,
                fail_body_fetch: false,
                last_modseq: None,
                snapshot: None,
                qresync_modseq_args: Vec::new(),
                abandon_on_body_fetch: false,
                abandoned: false,
                listable: None,
                list_calls: Vec::new(),
            }
        }
    }

    impl MailProvider for QresyncMock {
        fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
            Ok(vec![RemoteMailbox {
                path: "INBOX".into(),
                name: "INBOX".into(),
                role: Some("inbox".into()),
                role_source: Some(MailboxRoleSource::SpecialUse),
                delimiter: Some("/".into()),
                subscribed: true,
                selectable: true,
            }])
        }
        fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
            self.selects += 1;
            Ok(self.state)
        }
        fn fetch_new(
            &mut self,
            _after_uid: Option<i64>,
        ) -> Result<Vec<RemoteMessage>, ProviderError> {
            Ok(vec![])
        }
        fn fetch_new_descriptors(
            &mut self,
            _after_uid: Option<i64>,
        ) -> Result<Vec<RemoteMessageDescriptor>, ProviderError> {
            Ok(vec![])
        }
        fn logout(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }
        fn qresync_available(&self) -> bool {
            self.qresync_available
        }
        fn select_qresync(
            &mut self,
            _mailbox_path: &str,
            _uid_validity: i64,
            highest_modseq: i64,
        ) -> Result<QresyncSync, ProviderError> {
            self.select_qresyncs += 1;
            self.qresync_modseq_args.push(highest_modseq);
            self.outcome
                .clone()
                .ok_or_else(|| ProviderError::Unsupported("no outcome configured".into()))
        }
        fn fetch_bodies(&mut self, _uids: &[i64]) -> Result<Vec<RemoteMessage>, ProviderError> {
            if self.fail_body_fetch {
                self.abandoned = self.abandon_on_body_fetch;
                Err(ProviderError::Network("body fetch failed".into()))
            } else {
                Ok(vec![])
            }
        }
        fn last_highest_modseq(&self) -> Option<i64> {
            self.last_modseq
        }
        fn session_abandoned(&self) -> bool {
            self.abandoned
        }
        fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
            self.snapshot
                .clone()
                .ok_or_else(|| ProviderError::Unsupported("no snapshot configured".into()))
        }
        fn fetch_descriptor_list(
            &mut self,
            _mailbox_path: &str,
            uids: &[i64],
        ) -> Result<DescriptorChunkResult, ProviderError> {
            self.list_calls.push(uids.to_vec());
            let held = self
                .listable
                .clone()
                .ok_or_else(|| ProviderError::Unsupported("no listable configured".into()))?;
            Ok(DescriptorChunkResult {
                descriptors: held.into_iter().filter(|d| uids.contains(&d.uid)).collect(),
                skipped: vec![],
                incomplete: None,
            })
        }
    }

    #[test]
    fn a_matching_cursor_with_a_modseq_triggers_select_qresync_not_the_full_scan() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor_with_modseq(&db, 9, 100, 500);
        let mut mock = QresyncMock {
            qresync_available: true,
            state: MailboxState {
                uid_validity: 9,
                uid_next: 100,
                exists: 5,
            },
            selects: 0,
            select_qresyncs: 0,
            outcome: Some(QresyncSync {
                state: MailboxState {
                    uid_validity: 9,
                    uid_next: 100,
                    exists: 5,
                },
                highest_modseq: Some(600),
                changed: vec![],
                vanished: UidRangeSet::default(),
            }),
            fail_body_fetch: false,
            ..Default::default()
        };
        let report =
            sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
                .unwrap();
        assert_eq!(report.mailboxes[0].path, "INBOX");
        assert_eq!(mock.select_qresyncs, 1, "select_qresync must be called");
        assert_eq!(mock.selects, 0, "the plain select() path must not run too");
        let cursor = db.get_sync_cursor("acc", "acc:INBOX").unwrap().unwrap();
        assert_eq!(cursor.highest_modseq, Some(600));
    }

    /// S64 review fix: the skip-rebuild optimisation in `sync_with_provider`
    /// must not go stale when a LOCAL mutation (mark_read here) moved a
    /// thread aggregate without the server changing anything. A no-op
    /// QRESYNC resync (`changed`/`vanished` both empty — nothing fetched,
    /// nothing reconciled, nothing purged) would otherwise look identical
    /// before and after that mutation, and skip the rebuild both times.
    #[test]
    fn a_local_mark_read_forces_the_next_no_op_syncs_rebuild_but_only_that_one() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor_with_modseq(&db, 9, 100, 500);
        db.upsert_message(&descriptor_message(41)).unwrap();
        let no_op_outcome = || {
            Some(QresyncSync {
                state: MailboxState {
                    uid_validity: 9,
                    uid_next: 100,
                    exists: 1,
                },
                highest_modseq: Some(500),
                changed: vec![],
                vanished: UidRangeSet::default(),
            })
        };
        let mut mock = QresyncMock {
            qresync_available: true,
            state: MailboxState {
                uid_validity: 9,
                uid_next: 100,
                exists: 1,
            },
            outcome: no_op_outcome(),
            ..Default::default()
        };

        // Establish the thread projection once (no dirty marker yet, but
        // rebuild_threads is unconditional on direct calls — only sync's
        // OWN skip decision is under test here).
        db.rebuild_threads("acc").unwrap();
        assert_eq!(
            db.list_thread_summaries("acc", 10, 0).unwrap()[0].unread_count,
            1,
            "seeded unread"
        );

        // A genuinely no-op sync with no local mutation since the last
        // rebuild: nothing remote changed and the account isn't dirty, so
        // the rebuild must be skipped.
        sync_timing::enable();
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        let timings = sync_timing::take().unwrap();
        assert_eq!(
            timings
                .samples
                .get("db.rebuild_threads")
                .map(Vec::len)
                .unwrap_or(0),
            0,
            "a no-op sync with no local change must skip the rebuild"
        );

        // Explicit local mark_read: replays to the server (write-enabled),
        // but the server doesn't know yet — this sync's QRESYNC delta stays
        // empty either way. It must still mark the account dirty.
        db.mark_read("acc:INBOX:41", true).unwrap();
        assert_eq!(
            db.list_thread_summaries("acc", 10, 0).unwrap()[0].unread_count,
            1,
            "stale until the next rebuild"
        );

        mock.outcome = no_op_outcome();
        sync_timing::enable();
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        let timings = sync_timing::take().unwrap();
        assert_eq!(
            timings
                .samples
                .get("db.rebuild_threads")
                .map(Vec::len)
                .unwrap_or(0),
            1,
            "the dirty marker set by mark_read must force exactly one rebuild"
        );
        assert_eq!(
            db.list_thread_summaries("acc", 10, 0).unwrap()[0].unread_count,
            0,
            "the forced rebuild picks up the local mark_read"
        );

        // The rebuild just cleared the marker: a further no-op sync with no
        // new local change must go back to skipping.
        mock.outcome = no_op_outcome();
        sync_timing::enable();
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        let timings = sync_timing::take().unwrap();
        assert_eq!(
            timings
                .samples
                .get("db.rebuild_threads")
                .map(Vec::len)
                .unwrap_or(0),
            0,
            "the marker was cleared by the forced rebuild; a further no-op sync skips again"
        );
    }

    #[test]
    fn qresync_unavailable_falls_back_to_the_ordinary_select_path() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor_with_modseq(&db, 9, 100, 500);
        let mut mock = QresyncMock {
            qresync_available: false,
            state: MailboxState {
                uid_validity: 9,
                uid_next: 100,
                exists: 0,
            },
            selects: 0,
            select_qresyncs: 0,
            outcome: None,
            fail_body_fetch: false,
            ..Default::default()
        };
        let _ = sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
            .unwrap();
        // select_qresync was never eligible (qresync_available() is false),
        // so the mock's ordinary `select` counter — not select_qresync's —
        // is what advances.
        assert_eq!(mock.selects, 1);
        assert_eq!(mock.select_qresyncs, 0);
    }

    #[test]
    fn qresync_vanished_uids_purge_local_rows() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor_with_modseq(&db, 9, 100, 500);
        db.upsert_message(&descriptor_message(42)).unwrap();
        let mut mock = QresyncMock {
            qresync_available: true,
            state: MailboxState {
                uid_validity: 9,
                uid_next: 100,
                exists: 5,
            },
            selects: 0,
            select_qresyncs: 0,
            outcome: Some(QresyncSync {
                state: MailboxState {
                    uid_validity: 9,
                    uid_next: 100,
                    exists: 5,
                },
                highest_modseq: Some(600),
                changed: vec![],
                vanished: [42].into_iter().collect(),
            }),
            fail_body_fetch: false,
            ..Default::default()
        };
        let report =
            sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
                .unwrap();
        assert_eq!(report.mailboxes[0].vanished, 1);
        assert!(db.get_message_detail("acc:INBOX:42").unwrap().is_none());
    }

    /// Bug hunt 2026-10-12 (#3456): a body that fails to download must not
    /// stop the folder's reconciliation. The delta the SELECT carried is
    /// applied (the vanished UID is purged) and, since all of it was applied,
    /// HIGHESTMODSEQ advances — bodies are tracked by the missing-body queue,
    /// not by the modseq. The failure itself stays on record.
    #[test]
    fn a_body_fetch_failure_still_applies_the_qresync_delta() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor_with_modseq(&db, 9, 100, 500);
        db.upsert_message(&descriptor_message(42)).unwrap();
        db.upsert_message(&descriptor_message(43)).unwrap();
        let state = MailboxState {
            uid_validity: 9,
            uid_next: 100,
            exists: 5,
        };
        let mut mock = QresyncMock {
            qresync_available: true,
            state,
            outcome: Some(QresyncSync {
                state,
                highest_modseq: Some(600),
                changed: vec![UidFlags {
                    uid: 43,
                    is_seen: true,
                    is_flagged: false,
                    keywords: None,
                }],
                vanished: [42].into_iter().collect(),
            }),
            fail_body_fetch: true,
            ..Default::default()
        };
        // The same failure on every pass: the persistent case.
        for pass in 0..2 {
            let report =
                sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
                    .unwrap();
            assert!(
                report.mailboxes[0].error.is_some(),
                "pass {pass}: the body-fetch failure must be reported"
            );
            if pass == 0 {
                assert_eq!(report.mailboxes[0].vanished, 1);
                assert_eq!(report.mailboxes[0].flags_reconciled, 1);
            }
        }
        assert!(
            db.get_message_detail("acc:INBOX:42").unwrap().is_none(),
            "a UID the server expunged must be purged even though hydration failed"
        );
        assert!(
            db.get_message_detail("acc:INBOX:43")
                .unwrap()
                .unwrap()
                .header
                .is_read,
            "a server-side flag change must be applied even though hydration failed"
        );
        let cursor = db.get_sync_cursor("acc", "acc:INBOX").unwrap().unwrap();
        assert_eq!(
            cursor.highest_modseq,
            Some(600),
            "the whole delta was applied, so the modseq may advance"
        );
        let diag = db.list_mailbox_sync_states("acc").unwrap();
        let inbox = diag.iter().find(|d| d.mailbox_id == "acc:INBOX").unwrap();
        assert_eq!(
            inbox.last_error_phase.as_deref(),
            Some(phase::HYDRATE),
            "the HYDRATE failure must not be overwritten by a success"
        );
    }

    /// The same on a server without QRESYNC: the RFC 4549 full scan still
    /// runs after a failed body fetch.
    #[test]
    fn a_body_fetch_failure_still_runs_the_full_scan_reconciliation() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor(&db, 9, 100);
        db.upsert_message(&descriptor_message(42)).unwrap();
        db.upsert_message(&descriptor_message(43)).unwrap();
        let mut mock = QresyncMock {
            state: MailboxState {
                uid_validity: 9,
                uid_next: 100,
                exists: 1,
            },
            fail_body_fetch: true,
            snapshot: Some(vec![UidFlags {
                uid: 43,
                is_seen: true,
                is_flagged: false,
                keywords: None,
            }]),
            ..Default::default()
        };
        let report =
            sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
                .unwrap();
        assert!(report.mailboxes[0].error.is_some());
        assert_eq!(report.mailboxes[0].vanished, 1);
        assert_eq!(report.mailboxes[0].flags_reconciled, 1);
        assert!(db.get_message_detail("acc:INBOX:42").unwrap().is_none());
        assert!(
            db.get_message_detail("acc:INBOX:43")
                .unwrap()
                .unwrap()
                .header
                .is_read
        );
    }

    /// A body-fetch failure that poisoned the session (an undecodable
    /// response) still stops before reconciliation: nothing of the delta is
    /// applied, so HIGHESTMODSEQ keeps the baseline and the next
    /// `SELECT..QRESYNC` asks the server for that delta again.
    #[test]
    fn a_body_fetch_failure_that_abandons_the_session_withholds_the_modseq_advance() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor_with_modseq(&db, 9, 100, 500);
        db.upsert_message(&descriptor_message(42)).unwrap();
        let state = MailboxState {
            uid_validity: 9,
            uid_next: 100,
            exists: 5,
        };
        let mut mock = QresyncMock {
            qresync_available: true,
            state,
            outcome: Some(QresyncSync {
                state,
                highest_modseq: Some(600),
                changed: vec![],
                vanished: [42].into_iter().collect(),
            }),
            fail_body_fetch: true,
            abandon_on_body_fetch: true,
            ..Default::default()
        };
        let report =
            sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
                .unwrap();
        assert!(
            report.mailboxes[0].error.is_some(),
            "the body-fetch failure must be recorded"
        );
        assert!(
            db.get_message_detail("acc:INBOX:42").unwrap().is_some(),
            "nothing is reconciled over an abandoned session"
        );
        let cursor = db.get_sync_cursor("acc", "acc:INBOX").unwrap().unwrap();
        assert_eq!(
            cursor.highest_modseq,
            Some(500),
            "an unapplied delta must not advance the cursor past the last \
             successfully-reconciled modseq"
        );
    }

    /// A VANISHED UID that is skipped because a per-message op still pins it
    /// must ALSO withhold the modseq advance: QRESYNC only reports a vanished
    /// UID relative to the modseq baseline, so advancing the baseline while
    /// skipping the purge means the server never reports that UID again and
    /// it becomes a permanent ghost row (replay never calls `purge_message`).
    #[test]
    fn an_op_pinned_vanished_uid_withholds_the_qresync_modseq_advance() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor_with_modseq(&db, 9, 100, 500);
        db.upsert_message(&descriptor_message(42)).unwrap();
        db.mark_read("acc:INBOX:42", true).unwrap();
        let mut mock = QresyncMock {
            qresync_available: true,
            state: MailboxState {
                uid_validity: 9,
                uid_next: 100,
                exists: 5,
            },
            selects: 0,
            select_qresyncs: 0,
            outcome: Some(QresyncSync {
                state: MailboxState {
                    uid_validity: 9,
                    uid_next: 100,
                    exists: 5,
                },
                highest_modseq: Some(600),
                changed: vec![],
                vanished: [42].into_iter().collect(),
            }),
            fail_body_fetch: false,
            ..Default::default()
        };
        let report =
            sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
                .unwrap();
        assert_eq!(
            report.mailboxes[0].vanished, 0,
            "the pinned UID must not be purged yet"
        );
        assert!(
            db.get_message_detail("acc:INBOX:42").unwrap().is_some(),
            "a message pinned by a queued op must survive reconciliation"
        );
        let cursor = db.get_sync_cursor("acc", "acc:INBOX").unwrap().unwrap();
        assert_eq!(
            cursor.highest_modseq,
            Some(500),
            "the modseq must not advance while a VANISHED UID is still pinned by a \
             queued op, or the server will never report it again"
        );
    }

    // --- Checkpoint rule (S30 review round 3) -------------------------------
    //
    // The cursor's HIGHESTMODSEQ may advance only when every change up to it
    // was applied locally. Each test below either holds a change back (op pin
    // or flag lock, or an untrustworthy snapshot) and asserts the modseq stays
    // at its previous value, or holds nothing back and asserts it advances.

    fn inbox_state(uid_validity: i64, uid_next: i64, exists: i64) -> MailboxState {
        MailboxState {
            uid_validity,
            uid_next,
            exists,
        }
    }

    fn uid_flags(uid: i64, is_seen: bool, is_flagged: bool) -> UidFlags {
        UidFlags {
            keywords: None,
            uid,
            is_seen,
            is_flagged,
        }
    }

    /// A QRESYNC delta for `state`: the server's new HIGHESTMODSEQ plus the
    /// changed-FETCH and VANISHED items it piggy-backed on the SELECT.
    fn qresync_delta(
        state: MailboxState,
        highest_modseq: i64,
        changed: Vec<UidFlags>,
        vanished_uids: Vec<i64>,
    ) -> QresyncSync {
        QresyncSync {
            state,
            highest_modseq: Some(highest_modseq),
            changed,
            vanished: vanished_uids.into_iter().collect(),
        }
    }

    fn inbox_modseq(db: &Database) -> Option<i64> {
        db.get_sync_cursor("acc", "acc:INBOX")
            .unwrap()
            .expect("the INBOX cursor exists")
            .highest_modseq
    }

    /// Resolve every queued op as replayed (`succeeded`), the "op resolves"
    /// step between the two syncs of the tests below.
    fn replay_pending_ops(db: &Database) {
        for op in db.pending_operations("acc").unwrap() {
            db.set_operation_status(op.id, "succeeded", None).unwrap();
        }
    }

    /// The reviewer's blocking scenario. Every cursor written before S30 has
    /// `highest_modseq = None`, so the first post-upgrade sync on a QRESYNC
    /// server takes the plain `select()` (which captures HIGHESTMODSEQ once
    /// CONDSTORE is enabled) and the RFC 4549 full scan. If that scan keeps a
    /// vanished UID because a queued op pins it, persisting the captured
    /// modseq makes the NEXT sync QRESYNC-eligible from a point past the
    /// expunge: the server never re-reports the UID, and since only sync ever
    /// calls `purge_message`, the row is a ghost forever.
    #[test]
    fn post_upgrade_full_scan_keeps_a_pinned_vanished_uid_reachable_and_purges_it_once_resolved() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor(&db, 9, 100);
        db.upsert_message(&descriptor_message(41)).unwrap();
        db.upsert_message(&descriptor_message(42)).unwrap();
        db.mark_read("acc:INBOX:42", true).unwrap();
        let mut mock = QresyncMock {
            qresync_available: true,
            state: inbox_state(9, 100, 1),
            last_modseq: Some(700),
            // 42 was expunged server-side; 41 is still there.
            snapshot: Some(vec![uid_flags(41, false, false)]),
            ..Default::default()
        };

        let report =
            sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
                .unwrap();
        assert_eq!(
            (mock.selects, mock.select_qresyncs),
            (1, 0),
            "no modseq yet: plain select + full scan"
        );
        assert_eq!(report.mailboxes[0].vanished, 0);
        assert!(
            db.get_message_detail("acc:INBOX:42").unwrap().is_some(),
            "a message pinned by a queued op must survive reconciliation"
        );
        assert_eq!(
            inbox_modseq(&db),
            None,
            "the pinned vanished UID was held back, so the select's HIGHESTMODSEQ \
             must not be persisted — the previous value (None) stays"
        );

        replay_pending_ops(&db);
        let report =
            sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
                .unwrap();
        assert_eq!(
            (mock.selects, mock.select_qresyncs),
            (2, 0),
            "still no modseq, so still the full scan"
        );
        assert_eq!(report.mailboxes[0].vanished, 1);
        assert!(
            db.get_message_detail("acc:INBOX:42").unwrap().is_none(),
            "once the op resolved, the next sync must purge the row"
        );
        assert_eq!(inbox_modseq(&db), Some(700), "nothing held back: advance");
    }

    /// The full scan treats an empty snapshot of a non-empty mailbox as
    /// untrustworthy and skips the vanished pass for every local row — so it
    /// applied nothing for them and must not advance the modseq either.
    #[test]
    fn an_untrusted_empty_full_scan_snapshot_does_not_advance_the_modseq() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor(&db, 9, 100);
        db.upsert_message(&descriptor_message(41)).unwrap();
        let mut mock = QresyncMock {
            qresync_available: true,
            state: inbox_state(9, 100, 3),
            last_modseq: Some(700),
            snapshot: Some(vec![]),
            ..Default::default()
        };
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        assert!(db.get_message_detail("acc:INBOX:41").unwrap().is_some());
        assert_eq!(inbox_modseq(&db), None);
    }

    /// Scenario (2): a QRESYNC VANISHED UID held back by an op pin keeps the
    /// old baseline, so the next delta is requested from it, the server
    /// re-reports the UID, and — the op having resolved — it is purged.
    #[test]
    fn a_pinned_qresync_vanished_uid_is_rerequested_and_purged_once_the_op_resolves() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor_with_modseq(&db, 9, 100, 500);
        db.upsert_message(&descriptor_message(42)).unwrap();
        db.mark_read("acc:INBOX:42", true).unwrap();
        let state = inbox_state(9, 100, 5);
        let mut mock = QresyncMock {
            qresync_available: true,
            state,
            outcome: Some(qresync_delta(state, 600, vec![], vec![42])),
            ..Default::default()
        };

        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        assert!(db.get_message_detail("acc:INBOX:42").unwrap().is_some());
        assert_eq!(inbox_modseq(&db), Some(500));

        replay_pending_ops(&db);
        let report =
            sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
                .unwrap();
        assert_eq!(
            mock.qresync_modseq_args,
            vec![500, 500],
            "the second delta must be requested from the held-back baseline"
        );
        assert_eq!(report.mailboxes[0].vanished, 1);
        assert!(db.get_message_detail("acc:INBOX:42").unwrap().is_none());
        assert_eq!(inbox_modseq(&db), Some(600));
    }

    /// Scenario (3): a changed FETCH the delta could not apply because a
    /// queued `flag` op locks the star. The modseq stays put, so once the op
    /// has replayed the next delta is requested from the same baseline and
    /// the server's (now matching) value is applied before it advances.
    #[test]
    fn a_qresync_flag_change_held_back_by_a_lock_withholds_the_modseq_advance() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor_with_modseq(&db, 9, 100, 500);
        db.upsert_message(&descriptor_message(42)).unwrap();
        db.set_flagged("acc:INBOX:42", true).unwrap();
        let state = inbox_state(9, 100, 5);
        let mut mock = QresyncMock {
            qresync_available: true,
            state,
            // The server says 42 is NOT flagged; the queued op says it will be.
            outcome: Some(qresync_delta(
                state,
                600,
                vec![uid_flags(42, false, false)],
                vec![],
            )),
            ..Default::default()
        };

        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        let starred = |db: &Database| {
            db.list_local_uid_flags("acc", "acc:INBOX")
                .unwrap()
                .into_iter()
                .find(|(uid, _, _)| *uid == 42)
                .map(|(_, _, starred)| starred)
        };
        assert_eq!(starred(&db), Some(true), "the lock must be honoured");
        assert_eq!(
            inbox_modseq(&db),
            Some(500),
            "a flag change held back by a lock must withhold the modseq advance"
        );

        // The op replays: the server now has 42 flagged, and reports it
        // again because the next delta starts from the held-back 500.
        replay_pending_ops(&db);
        mock.outcome = Some(qresync_delta(
            state,
            700,
            vec![uid_flags(42, false, true)],
            vec![],
        ));
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        assert_eq!(mock.qresync_modseq_args, vec![500, 500]);
        assert_eq!(starred(&db), Some(true));
        assert_eq!(inbox_modseq(&db), Some(700));
    }

    /// The same held-back change, but the op is cancelled instead of
    /// replayed. The server may never touch 42 again, so a re-report cannot
    /// be relied on: the cancel invalidates the baseline and the next sync's
    /// full scan converges the star to the server's value.
    /// A move op for `acc:INBOX:uid` exactly as `enqueue_move` writes it.
    fn queue_archive(db: &Database, uid: i64, uid_validity: i64) -> i64 {
        db.upsert_mailbox(&Mailbox {
            id: "acc:Archive".into(),
            account_id: "acc".into(),
            path: "Archive".into(),
            name: "Archive".into(),
            role: Some("archive".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.set_sync_cursor(
            "acc",
            "acc:INBOX",
            &SyncCursor {
                uid_validity: Some(uid_validity),
                uid_next: Some(100),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
        db.archive_message(&format!("acc:INBOX:{uid}")).unwrap();
        db.pending_operations("acc")
            .unwrap()
            .into_iter()
            .find(|o| o.kind == "move")
            .unwrap()
            .id
    }

    /// Bug hunt #69: cancelling an archive left the message invisible locally
    /// while it was still in the Inbox on the server. The next sync brings it
    /// back (and only it), as old mail that rules never see.
    #[test]
    fn a_cancelled_archive_comes_back_on_the_next_sync() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor(&db, 9, 100);
        db.upsert_message(&descriptor_message(42)).unwrap();
        db.upsert_message(&descriptor_message(43)).unwrap();
        let op = queue_archive(&db, 42, 9);
        assert!(
            db.get_message_detail("acc:INBOX:42").unwrap().is_none(),
            "row dropped up front"
        );
        db.cancel_operation(op).unwrap();
        assert!(
            db.get_message_detail("acc:INBOX:42").unwrap().is_none(),
            "still hidden after cancel"
        );

        let mut mock = QresyncMock {
            state: inbox_state(9, 100, 2),
            listable: Some(vec![
                remote_descriptor(42, "Archived by mistake"),
                remote_descriptor(43, "Other"),
            ]),
            ..Default::default()
        };
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        assert_eq!(
            mock.list_calls,
            vec![vec![42]],
            "only the cancelled UID is re-fetched"
        );
        let back = db
            .get_message_detail("acc:INBOX:42")
            .unwrap()
            .expect("message is back");
        assert_eq!(back.header.subject.as_deref(), Some("Archived by mistake"));
        // The set is consumed: a second pass fetches nothing.
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        assert_eq!(mock.list_calls.len(), 1);
    }

    #[test]
    fn a_cancelled_move_under_another_uidvalidity_is_not_refetched() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor(&db, 9, 100);
        db.upsert_message(&descriptor_message(42)).unwrap();
        let op = queue_archive(&db, 42, 9);
        db.cancel_operation(op).unwrap();
        // The server rebuilt the mailbox: UID 42 names another message now.
        let mut mock = QresyncMock {
            state: inbox_state(10, 100, 1),
            listable: Some(vec![remote_descriptor(42, "Someone else")]),
            ..Default::default()
        };
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        assert!(mock.list_calls.is_empty());
        assert!(
            db.pending_refetch_uids("acc", "INBOX", 9, 10)
                .unwrap()
                .is_empty(),
            "stale rows dropped"
        );
    }

    #[test]
    fn a_cancelled_archive_whose_message_left_the_server_just_clears_the_row() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor(&db, 9, 100);
        db.upsert_message(&descriptor_message(42)).unwrap();
        let op = queue_archive(&db, 42, 9);
        db.cancel_operation(op).unwrap();
        let mut mock = QresyncMock {
            state: inbox_state(9, 100, 0),
            listable: Some(vec![]),
            ..Default::default()
        };
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        assert!(
            mock.list_calls.is_empty(),
            "an empty mailbox has nothing to re-fetch"
        );
        assert!(db.get_message_detail("acc:INBOX:42").unwrap().is_none());
        assert!(db
            .pending_refetch_uids("acc", "INBOX", 9, 10)
            .unwrap()
            .is_empty());
    }

    /// A provider that cannot list descriptors leaves the rows for later.
    #[test]
    fn refetch_waits_when_the_provider_cannot_list_descriptors() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor(&db, 9, 100);
        db.upsert_message(&descriptor_message(42)).unwrap();
        let op = queue_archive(&db, 42, 9);
        db.cancel_operation(op).unwrap();
        let mut mock = QresyncMock {
            state: inbox_state(9, 100, 1),
            listable: None,
            ..Default::default()
        };
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        assert_eq!(
            db.pending_refetch_uids("acc", "INBOX", 9, 10).unwrap(),
            vec![42]
        );
    }

    #[test]
    fn a_lock_held_change_whose_op_is_cancelled_converges_via_the_full_scan() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor_with_modseq(&db, 9, 100, 500);
        db.upsert_message(&descriptor_message(42)).unwrap();
        db.set_flagged("acc:INBOX:42", true).unwrap();
        let state = inbox_state(9, 100, 5);
        let mut mock = QresyncMock {
            qresync_available: true,
            state,
            outcome: Some(qresync_delta(
                state,
                600,
                vec![uid_flags(42, false, false)],
                vec![],
            )),
            last_modseq: Some(600),
            snapshot: Some(vec![uid_flags(42, false, false)]),
            ..Default::default()
        };
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        assert_eq!(inbox_modseq(&db), Some(500));

        for op in db.pending_operations("acc").unwrap() {
            db.cancel_operation(op.id).unwrap();
        }
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        assert_eq!((mock.selects, mock.select_qresyncs), (1, 1));
        let starred = db
            .list_local_uid_flags("acc", "acc:INBOX")
            .unwrap()
            .into_iter()
            .find(|(uid, _, _)| *uid == 42)
            .map(|(_, _, starred)| starred);
        assert_eq!(
            starred,
            Some(false),
            "the server's value wins after a cancel"
        );
        assert_eq!(inbox_modseq(&db), Some(600));
    }

    /// The same rule on the full-scan path: a read flag the scan could not
    /// converge because a queued `mark_read` locks it.
    #[test]
    fn a_full_scan_flag_change_held_back_by_a_lock_withholds_the_modseq_advance() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor(&db, 9, 100);
        db.upsert_message(&descriptor_message(41)).unwrap();
        db.upsert_message(&descriptor_message(42)).unwrap();
        db.mark_read("acc:INBOX:42", true).unwrap();
        let mut mock = QresyncMock {
            qresync_available: true,
            state: inbox_state(9, 100, 2),
            last_modseq: Some(700),
            snapshot: Some(vec![
                uid_flags(41, false, false),
                uid_flags(42, false, false),
            ]),
            ..Default::default()
        };
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        assert_eq!(inbox_modseq(&db), None);
    }

    /// Scenario (4), no regression: a lock only holds the modseq back when it
    /// actually kept a server value from applying. A locked flag that already
    /// matches the server, plus an ordinary unlocked change, advance it.
    #[test]
    fn a_lock_that_held_nothing_back_lets_the_qresync_modseq_advance() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor_with_modseq(&db, 9, 100, 500);
        db.upsert_message(&descriptor_message(42)).unwrap();
        db.upsert_message(&descriptor_message(43)).unwrap();
        db.set_flagged("acc:INBOX:42", true).unwrap();
        let state = inbox_state(9, 100, 5);
        let mut mock = QresyncMock {
            qresync_available: true,
            state,
            outcome: Some(qresync_delta(
                state,
                600,
                vec![uid_flags(42, false, true), uid_flags(43, true, false)],
                vec![],
            )),
            ..Default::default()
        };
        let report =
            sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
                .unwrap();
        assert_eq!(report.mailboxes[0].flags_reconciled, 1);
        assert_eq!(inbox_modseq(&db), Some(600));
    }

    /// Scenario (4) on the full-scan path: an unpinned vanished UID and an
    /// unlocked flag change both apply, so the select's capture is persisted.
    #[test]
    fn a_clean_full_scan_persists_the_selects_highest_modseq() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor(&db, 9, 100);
        db.upsert_message(&descriptor_message(41)).unwrap();
        db.upsert_message(&descriptor_message(42)).unwrap();
        let mut mock = QresyncMock {
            qresync_available: true,
            state: inbox_state(9, 100, 1),
            last_modseq: Some(700),
            snapshot: Some(vec![uid_flags(41, true, false)]),
            ..Default::default()
        };
        let report =
            sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
                .unwrap();
        assert_eq!(report.mailboxes[0].vanished, 1);
        assert_eq!(report.mailboxes[0].flags_reconciled, 1);
        assert_eq!(inbox_modseq(&db), Some(700));
    }

    // -- S42: mark-read on arrival in a muted thread (Task 6) -----------

    #[test]
    fn new_message_in_muted_thread_is_marked_read() {
        let db = temp_db();
        let mut mock = MockProvider {
            state: MailboxState {
                uid_validity: 1,
                uid_next: 2,
                exists: 1,
            },
            messages: vec![RemoteMessage {
                uid: 1,
                raw: "From: Alice <alice@example.com>\r\n\
                     To: test@local.test\r\n\
                     Subject: Topic\r\n\
                     Message-ID: <root@example.com>\r\n\
                     Date: Tue, 1 Jul 2025 10:00:00 +0000\r\n\
                     \r\n\
                     Body 1.\r\n"
                    .as_bytes()
                    .to_vec(),
                is_seen: false,
                is_flagged: false,
            }],
        };
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        db.mute_thread("acc:INBOX:1", true).unwrap();
        db.set_account_write_enabled("acc", true).unwrap();

        // A reply lands in the next sync pass; write-enabled, so exactly one
        // mark_read op is queued for the auto-read.
        mock.messages.push(RemoteMessage {
            uid: 2,
            raw: "From: Bob <bob@example.com>\r\n\
                 To: test@local.test\r\n\
                 Subject: Re: Topic\r\n\
                 Message-ID: <reply@example.com>\r\n\
                 In-Reply-To: <root@example.com>\r\n\
                 References: <root@example.com>\r\n\
                 Date: Tue, 1 Jul 2025 11:00:00 +0000\r\n\
                 \r\n\
                 Body 2.\r\n"
                .as_bytes()
                .to_vec(),
            is_seen: false,
            is_flagged: false,
        });
        mock.state.uid_next = 3;
        mock.state.exists = 2;
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();

        let detail = db.get_message_detail("acc:INBOX:2").unwrap().unwrap();
        assert!(
            detail.header.is_read,
            "a new message in a muted thread must be marked read on arrival"
        );
        let ops: Vec<_> = db
            .pending_operations("acc")
            .unwrap()
            .into_iter()
            .filter(|op| op.kind == "mark_read")
            .collect();
        assert_eq!(
            ops.len(),
            1,
            "write-enabled: exactly one mark_read op queued"
        );
    }

    #[test]
    fn new_message_in_muted_thread_on_evaluation_mode_account_queues_no_op() {
        let db = temp_db();
        let mut mock = MockProvider {
            state: MailboxState {
                uid_validity: 1,
                uid_next: 2,
                exists: 1,
            },
            messages: vec![RemoteMessage {
                uid: 1,
                raw: "From: Alice <alice@example.com>\r\n\
                     To: test@local.test\r\n\
                     Subject: Topic\r\n\
                     Message-ID: <root2@example.com>\r\n\
                     Date: Tue, 1 Jul 2025 10:00:00 +0000\r\n\
                     \r\n\
                     Body 1.\r\n"
                    .as_bytes()
                    .to_vec(),
                is_seen: false,
                is_flagged: false,
            }],
        };
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        db.mute_thread("acc:INBOX:1", true).unwrap();
        db.set_account_write_enabled("acc", false).unwrap();

        mock.messages.push(RemoteMessage {
            uid: 2,
            raw: "From: Bob <bob@example.com>\r\n\
                 To: test@local.test\r\n\
                 Subject: Re: Topic\r\n\
                 Message-ID: <reply2@example.com>\r\n\
                 In-Reply-To: <root2@example.com>\r\n\
                 References: <root2@example.com>\r\n\
                 Date: Tue, 1 Jul 2025 11:00:00 +0000\r\n\
                 \r\n\
                 Body 2.\r\n"
                .as_bytes()
                .to_vec(),
            is_seen: false,
            is_flagged: false,
        });
        mock.state.uid_next = 3;
        mock.state.exists = 2;
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();

        let detail = db.get_message_detail("acc:INBOX:2").unwrap().unwrap();
        assert!(
            detail.header.is_read,
            "the local read flag still flips even in evaluation mode"
        );
        assert!(
            db.pending_operations("acc")
                .unwrap()
                .iter()
                .all(|op| op.kind != "mark_read"),
            "evaluation mode: zero mark_read ops queued"
        );
    }

    // -- S42: keyword ingest and reconcile (Task 4) --------------------

    /// A scriptable provider whose `fetch_new_descriptors`/`list_uid_flags`
    /// both derive from one descriptor list, so a keyword set on a
    /// descriptor is exactly what the first-sync ingest path sees.
    struct DescriptorKeywordMock {
        descriptors: Vec<RemoteMessageDescriptor>,
    }

    impl MailProvider for DescriptorKeywordMock {
        fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
            Ok(vec![rmb(
                "INBOX",
                "INBOX",
                Some("inbox"),
                Some("/"),
                true,
                true,
            )])
        }
        fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
            Ok(MailboxState {
                uid_validity: 1,
                uid_next: (self.descriptors.iter().map(|d| d.uid).max().unwrap_or(0) + 1),
                exists: self.descriptors.len() as i64,
            })
        }
        fn fetch_new(
            &mut self,
            _after_uid: Option<i64>,
        ) -> Result<Vec<RemoteMessage>, ProviderError> {
            Err(ProviderError::Protocol("body fetch should not run".into()))
        }
        fn fetch_new_descriptors(
            &mut self,
            after_uid: Option<i64>,
        ) -> Result<Vec<RemoteMessageDescriptor>, ProviderError> {
            Ok(self
                .descriptors
                .iter()
                .filter(|d| after_uid.is_none_or(|uid| d.uid > uid))
                .cloned()
                .collect())
        }
        fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
            Ok(self
                .descriptors
                .iter()
                .map(|d| UidFlags {
                    keywords: d.keywords.clone(),
                    uid: d.uid,
                    is_seen: d.is_seen,
                    is_flagged: d.is_flagged,
                })
                .collect())
        }
        fn logout(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    #[test]
    fn first_sync_stores_descriptor_keywords() {
        let db = temp_db();
        let mut mock = DescriptorKeywordMock {
            descriptors: vec![RemoteMessageDescriptor {
                keywords: Some(vec!["Work".to_string()]),
                ..remote_descriptor(1, "Hello")
            }],
        };
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        let map = db
            .keywords_for_messages(&["acc:INBOX:1".to_string()])
            .unwrap();
        assert_eq!(
            map.get("acc:INBOX:1").cloned().unwrap_or_default(),
            vec!["Work".to_string()]
        );
    }

    #[test]
    fn resync_full_scan_replaces_server_keywords_but_keeps_local() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor(&db, 9, 100);
        db.upsert_message(&descriptor_message(41)).unwrap();
        seed_local_keyword(&db, "acc:INBOX:41", "Local");
        let mut mock = QresyncMock {
            qresync_available: true,
            state: inbox_state(9, 100, 1),
            last_modseq: None,
            snapshot: Some(vec![UidFlags {
                keywords: Some(vec!["$Label1".to_string()]),
                uid: 41,
                is_seen: false,
                is_flagged: false,
            }]),
            ..Default::default()
        };
        let report =
            sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
                .unwrap();
        assert_eq!(report.mailboxes[0].flags_reconciled, 1);
        let mut got = db
            .keywords_for_messages(&["acc:INBOX:41".to_string()])
            .unwrap()
            .remove("acc:INBOX:41")
            .unwrap_or_default();
        got.sort();
        assert_eq!(got, vec!["$Label1".to_string(), "Local".to_string()]);
    }

    #[test]
    fn resync_with_none_keywords_leaves_local_state() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor(&db, 9, 100);
        db.upsert_message(&descriptor_message(41)).unwrap();
        seed_local_keyword(&db, "acc:INBOX:41", "Local");
        let mut mock = QresyncMock {
            qresync_available: true,
            state: inbox_state(9, 100, 1),
            last_modseq: None,
            snapshot: Some(vec![uid_flags(41, false, false)]),
            ..Default::default()
        };
        let report =
            sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
                .unwrap();
        assert_eq!(
            report.mailboxes[0].flags_reconciled, 0,
            "remote reported no keywords at all (None) — nothing must change"
        );
        let got = db
            .keywords_for_messages(&["acc:INBOX:41".to_string()])
            .unwrap()
            .remove("acc:INBOX:41")
            .unwrap_or_default();
        assert_eq!(got, vec!["Local".to_string()]);
    }

    #[test]
    fn queued_keyword_op_locks_keywords_only() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor(&db, 9, 100);
        db.upsert_message(&descriptor_message(42)).unwrap();
        db.set_message_keyword("acc:INBOX:42", "Old", true).unwrap();
        let mut mock = QresyncMock {
            qresync_available: true,
            state: inbox_state(9, 100, 1),
            last_modseq: None,
            snapshot: Some(vec![UidFlags {
                keywords: Some(vec![]),
                uid: 42,
                is_seen: true,
                is_flagged: false,
            }]),
            ..Default::default()
        };
        let report =
            sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
                .unwrap();
        assert_eq!(
            report.mailboxes[0].flags_reconciled, 1,
            "\\Seen still reconciles"
        );
        let got = db
            .keywords_for_messages(&["acc:INBOX:42".to_string()])
            .unwrap()
            .remove("acc:INBOX:42")
            .unwrap_or_default();
        assert_eq!(
            got,
            vec!["Old".to_string()],
            "the queued keyword op must keep the keyword untouched"
        );
    }

    #[test]
    fn qresync_delta_applies_keyword_changes() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor_with_modseq(&db, 9, 100, 500);
        db.upsert_message(&descriptor_message(42)).unwrap();
        let state = inbox_state(9, 100, 1);
        let mut mock = QresyncMock {
            qresync_available: true,
            state,
            outcome: Some(qresync_delta(
                state,
                600,
                vec![UidFlags {
                    keywords: Some(vec!["Later".to_string()]),
                    uid: 42,
                    is_seen: false,
                    is_flagged: false,
                }],
                vec![],
            )),
            ..Default::default()
        };
        let report =
            sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
                .unwrap();
        assert_eq!(report.mailboxes[0].flags_reconciled, 1);
        let got = db
            .keywords_for_messages(&["acc:INBOX:42".to_string()])
            .unwrap()
            .remove("acc:INBOX:42")
            .unwrap_or_default();
        assert_eq!(got, vec!["Later".to_string()]);
        assert_eq!(inbox_modseq(&db), Some(600));
    }

    /// A modseq belongs to one UID space. After a UIDVALIDITY change the old
    /// cursor's modseq describes a different mailbox, so "keep the previous
    /// modseq" must mean None there — persisting the old space's 900 against
    /// the new UIDVALIDITY would make a later QRESYNC skip every change the
    /// new space made below 900. Since, a reset also SKIPS
    /// full-scan reconciliation entirely (the purge-and-refetch already
    /// covers it), so nothing is held back and the new space's own reported
    /// modseq (50) persists — never the stale 900, and not held back to
    /// None either, because there is nothing left to fail at.
    #[test]
    fn a_uidvalidity_reset_persists_the_new_spaces_modseq_never_the_old_ones() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor_with_modseq(&db, 9, 100, 900);
        db.upsert_message(&descriptor_message(42)).unwrap();
        let new_space = inbox_state(10, 5, 1);
        let mut mock = QresyncMock {
            qresync_available: true,
            state: new_space,
            // The server ignored the stale QRESYNC parameters (new UIDVALIDITY).
            outcome: Some(qresync_delta(new_space, 50, vec![], vec![])),
            last_modseq: Some(50),
            // Would fail if called — proves #2502's skip means it never is.
            snapshot: None,
            ..Default::default()
        };
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        let cursor = db.get_sync_cursor("acc", "acc:INBOX").unwrap().unwrap();
        assert_eq!(cursor.uid_validity, Some(10));
        assert_eq!(
            cursor.highest_modseq,
            Some(50),
            "the new space's own modseq persists once reconciliation is (rightly) \
             skipped after a from-scratch reset — never the old cursor's 900"
        );
    }

    /// A whole-UID-space VANISHED range is reconciled against the LOCAL
    /// rows (bounded by what this client actually has), never walked UID by
    /// UID: every unpinned local row in it is purged, a pinned one is held
    /// back (and so holds the modseq back), and the pass finishes promptly.
    #[test]
    fn a_whole_uid_space_vanished_range_is_intersected_with_local_rows() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor_with_modseq(&db, 9, 100, 500);
        db.upsert_message(&descriptor_message(41)).unwrap();
        db.upsert_message(&descriptor_message(42)).unwrap();
        db.mark_read("acc:INBOX:42", true).unwrap();
        let state = inbox_state(9, 100, 5);
        let mut mock = QresyncMock {
            qresync_available: true,
            state,
            outcome: Some(QresyncSync {
                state,
                highest_modseq: Some(600),
                changed: vec![],
                vanished: UidRangeSet::from_ranges(std::iter::once(1..=i64::from(u32::MAX))),
            }),
            ..Default::default()
        };
        let started = std::time::Instant::now();
        let report =
            sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
                .unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "reconciliation must scale with local rows, not the range's width"
        );
        assert_eq!(report.mailboxes[0].vanished, 1);
        assert!(db.get_message_detail("acc:INBOX:41").unwrap().is_none());
        assert!(db.get_message_detail("acc:INBOX:42").unwrap().is_some());
        assert_eq!(inbox_modseq(&db), Some(500));
    }

    /// The full-scan hold-back from a POSITIVE baseline (the tests above all
    /// start from `None`, which a "persist None when held back" bug would
    /// also satisfy): the previous modseq itself must survive.
    #[test]
    fn a_full_scan_hold_back_keeps_a_positive_baseline() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor_with_modseq(&db, 9, 100, 500);
        db.upsert_message(&descriptor_message(41)).unwrap();
        db.upsert_message(&descriptor_message(42)).unwrap();
        db.mark_read("acc:INBOX:42", true).unwrap();
        let mut mock = QresyncMock {
            // No QRESYNC this session, so the positive cursor takes the scan.
            qresync_available: false,
            state: inbox_state(9, 100, 1),
            last_modseq: Some(700),
            snapshot: Some(vec![uid_flags(41, false, false)]),
            ..Default::default()
        };
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        assert_eq!((mock.selects, mock.select_qresyncs), (1, 0));
        assert!(db.get_message_detail("acc:INBOX:42").unwrap().is_some());
        assert_eq!(inbox_modseq(&db), Some(500));
    }

    /// The emptied-mailbox arm (`exists == 0`, same UID space) under the
    /// same rule: a row it keeps because an op pins it holds the modseq at
    /// the baseline, and is purged once the op resolves.
    #[test]
    fn an_emptied_mailbox_with_a_pinned_row_holds_the_modseq_until_the_op_resolves() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor_with_modseq(&db, 9, 100, 500);
        db.upsert_message(&descriptor_message(41)).unwrap();
        db.upsert_message(&descriptor_message(42)).unwrap();
        db.mark_read("acc:INBOX:42", true).unwrap();
        let state = inbox_state(9, 100, 0);
        let mut mock = QresyncMock {
            qresync_available: true,
            state,
            outcome: Some(qresync_delta(state, 600, vec![], vec![41, 42])),
            ..Default::default()
        };
        let report =
            sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
                .unwrap();
        assert_eq!(report.mailboxes[0].vanished, 1);
        assert!(db.get_message_detail("acc:INBOX:42").unwrap().is_some());
        assert_eq!(inbox_modseq(&db), Some(500));

        replay_pending_ops(&db);
        let report =
            sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
                .unwrap();
        assert_eq!(mock.qresync_modseq_args, vec![500, 500]);
        assert_eq!(report.mailboxes[0].vanished, 1);
        assert!(db.get_message_detail("acc:INBOX:42").unwrap().is_none());
        assert_eq!(inbox_modseq(&db), Some(600));
    }

    fn local_read(db: &Database, uid: i64) -> Option<bool> {
        db.list_local_uid_flags("acc", "acc:INBOX")
            .unwrap()
            .into_iter()
            .find(|(u, _, _)| *u == uid)
            .map(|(_, read, _)| read)
    }

    /// QRESYNC only reports what the SERVER changed. An evaluation-mode
    /// auto-read flips the local flag with no op queued, and the documented
    /// outcome is that a later sync reverts it to the server's truth — which
    /// only the full scan can see. The local change must therefore force one.
    #[test]
    fn an_evaluation_mode_auto_read_forces_a_full_scan_that_reverts_it() {
        let db = temp_db();
        seed_account_mailbox(&db);
        db.set_account_write_enabled("acc", false).unwrap();
        seed_inbox_cursor_with_modseq(&db, 9, 100, 500);
        db.upsert_message(&descriptor_message(42)).unwrap();
        db.mark_read_auto("acc:INBOX:42", true).unwrap();
        let state = inbox_state(9, 100, 1);
        let mut mock = QresyncMock {
            qresync_available: true,
            state,
            // The server never touched 42: the delta is empty.
            outcome: Some(qresync_delta(state, 600, vec![], vec![])),
            last_modseq: Some(600),
            snapshot: Some(vec![uid_flags(42, false, false)]),
            ..Default::default()
        };
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        assert_eq!(
            (mock.selects, mock.select_qresyncs),
            (1, 0),
            "a local-only change must force the full scan"
        );
        assert_eq!(local_read(&db, 42), Some(false), "reverted to the server");
        assert_eq!(inbox_modseq(&db), Some(600));
    }

    /// A queued op whose local flag the server never saw, then cancelled: the
    /// QRESYNC sync that ran while it was queued had nothing to hold back
    /// (the server did not change), so only the cancel can tell the next
    /// sync that local state now diverges.
    #[test]
    fn a_cancelled_op_forces_a_full_scan_that_reverts_its_local_flag() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor_with_modseq(&db, 9, 100, 500);
        db.upsert_message(&descriptor_message(42)).unwrap();
        db.mark_read("acc:INBOX:42", true).unwrap();
        let state = inbox_state(9, 100, 1);
        let mut mock = QresyncMock {
            qresync_available: true,
            state,
            outcome: Some(qresync_delta(state, 600, vec![], vec![])),
            last_modseq: Some(600),
            snapshot: Some(vec![uid_flags(42, false, false)]),
            ..Default::default()
        };
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        assert_eq!(mock.select_qresyncs, 1);
        assert_eq!(inbox_modseq(&db), Some(600), "nothing was held back");

        for op in db.pending_operations("acc").unwrap() {
            db.cancel_operation(op.id).unwrap();
        }
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        assert_eq!(
            (mock.selects, mock.select_qresyncs),
            (1, 1),
            "the cancel must force the full scan"
        );
        assert_eq!(local_read(&db, 42), Some(false));
        assert_eq!(inbox_modseq(&db), Some(600));
    }

    /// Wraps `QresyncMock` and makes a local-only change (an evaluation-mode
    /// auto-read) land in the middle of a sync, during body hydration — after
    /// the cursor was read and the checkpoint written, before the final
    /// cursor write.
    struct MidRunLocalChange<'a> {
        db: &'a Database,
        inner: QresyncMock,
    }

    impl MailProvider for MidRunLocalChange<'_> {
        fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
            self.inner.list_mailboxes()
        }
        fn select(&mut self, path: &str) -> Result<MailboxState, ProviderError> {
            self.inner.select(path)
        }
        fn fetch_new(&mut self, after: Option<i64>) -> Result<Vec<RemoteMessage>, ProviderError> {
            self.inner.fetch_new(after)
        }
        fn fetch_new_descriptors(
            &mut self,
            after: Option<i64>,
        ) -> Result<Vec<RemoteMessageDescriptor>, ProviderError> {
            self.inner.fetch_new_descriptors(after)
        }
        fn logout(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }
        fn qresync_available(&self) -> bool {
            self.inner.qresync_available()
        }
        fn select_qresync(
            &mut self,
            path: &str,
            uid_validity: i64,
            modseq: i64,
        ) -> Result<QresyncSync, ProviderError> {
            self.inner.select_qresync(path, uid_validity, modseq)
        }
        fn last_highest_modseq(&self) -> Option<i64> {
            self.inner.last_highest_modseq()
        }
        fn fetch_bodies(&mut self, uids: &[i64]) -> Result<Vec<RemoteMessage>, ProviderError> {
            self.db.mark_read_auto("acc:INBOX:42", true).unwrap();
            self.inner.fetch_bodies(uids)
        }
    }

    // --- bug hunt 2026-10-12 (#3456): ops queued while a pass is running ----

    /// Wraps `QresyncMock` and runs `hook` once, when the server snapshot for
    /// INBOX is taken (`select_qresync` or `list_uid_flags`) — i.e. after the
    /// pass built its op guards and before it reconciles the folder.
    struct MidPassOp<'a> {
        db: &'a Database,
        inner: QresyncMock,
        hook: Option<fn(&Database)>,
    }

    impl MidPassOp<'_> {
        fn fire(&mut self) {
            if let Some(hook) = self.hook.take() {
                hook(self.db);
            }
        }
    }

    impl MailProvider for MidPassOp<'_> {
        fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
            self.inner.list_mailboxes()
        }
        fn select(&mut self, path: &str) -> Result<MailboxState, ProviderError> {
            self.inner.select(path)
        }
        fn fetch_new(&mut self, after: Option<i64>) -> Result<Vec<RemoteMessage>, ProviderError> {
            self.inner.fetch_new(after)
        }
        fn fetch_new_descriptors(
            &mut self,
            after: Option<i64>,
        ) -> Result<Vec<RemoteMessageDescriptor>, ProviderError> {
            self.inner.fetch_new_descriptors(after)
        }
        fn logout(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }
        fn qresync_available(&self) -> bool {
            self.inner.qresync_available()
        }
        fn select_qresync(
            &mut self,
            path: &str,
            uid_validity: i64,
            modseq: i64,
        ) -> Result<QresyncSync, ProviderError> {
            self.fire();
            self.inner.select_qresync(path, uid_validity, modseq)
        }
        fn last_highest_modseq(&self) -> Option<i64> {
            self.inner.last_highest_modseq()
        }
        fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
            self.fire();
            self.inner.list_uid_flags()
        }
    }

    fn mark_42_read(db: &Database) {
        db.mark_read("acc:INBOX:42", true).unwrap();
    }

    fn is_read(db: &Database, id: &str) -> bool {
        db.get_message_detail(id).unwrap().unwrap().header.is_read
    }

    fn run_mid_pass(db: &Database, provider: &mut MidPassOp<'_>) -> SyncReport {
        sync_with_provider(db, "acc", "test@local.test", None, provider, "test-host").unwrap()
    }

    #[test]
    fn a_mark_read_queued_mid_pass_is_not_reverted_by_the_full_scan() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor(&db, 9, 100);
        db.upsert_message(&descriptor_message(42)).unwrap();
        let mut provider = MidPassOp {
            db: &db,
            inner: QresyncMock {
                state: inbox_state(9, 100, 1),
                // The server has not seen the mark-read yet.
                snapshot: Some(vec![uid_flags(42, false, false)]),
                ..Default::default()
            },
            hook: Some(mark_42_read),
        };
        let report = run_mid_pass(&db, &mut provider);
        assert!(provider.hook.is_none(), "the op was queued during the pass");
        assert_eq!(report.mailboxes[0].flags_reconciled, 0);
        assert!(
            is_read(&db, "acc:INBOX:42"),
            "a mark-read queued while the pass was running must not flip back"
        );
        assert_eq!(db.pending_operations("acc").unwrap().len(), 1);
    }

    #[test]
    fn a_mark_read_queued_mid_pass_is_not_reverted_by_the_qresync_delta() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor_with_modseq(&db, 9, 100, 500);
        db.upsert_message(&descriptor_message(42)).unwrap();
        let state = inbox_state(9, 100, 1);
        let mut provider = MidPassOp {
            db: &db,
            inner: QresyncMock {
                qresync_available: true,
                state,
                outcome: Some(qresync_delta(
                    state,
                    600,
                    vec![uid_flags(42, false, true)],
                    vec![],
                )),
                ..Default::default()
            },
            hook: Some(mark_42_read),
        };
        let report = run_mid_pass(&db, &mut provider);
        assert!(is_read(&db, "acc:INBOX:42"));
        // The star half of the same server change is not locked: it applies.
        assert_eq!(report.mailboxes[0].flags_reconciled, 1);
        assert!(
            db.get_message_detail("acc:INBOX:42")
                .unwrap()
                .unwrap()
                .header
                .is_starred
        );
        assert!(
            !inbox_modseq(&db).is_some_and(|m| m >= 600),
            "a held-back flag must not let the modseq advance past it, got {:?}",
            inbox_modseq(&db)
        );
    }

    #[test]
    fn an_op_queued_mid_pass_pins_a_uid_the_server_reports_vanished() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor_with_modseq(&db, 9, 100, 500);
        db.upsert_message(&descriptor_message(42)).unwrap();
        let state = inbox_state(9, 100, 1);
        let mut provider = MidPassOp {
            db: &db,
            inner: QresyncMock {
                qresync_available: true,
                state,
                outcome: Some(qresync_delta(state, 600, vec![], vec![42])),
                ..Default::default()
            },
            hook: Some(mark_42_read),
        };
        let report = run_mid_pass(&db, &mut provider);
        assert_eq!(report.mailboxes[0].vanished, 0);
        assert!(
            db.get_message_detail("acc:INBOX:42").unwrap().is_some(),
            "a message with an op queued mid-pass must not be purged from under it"
        );
    }

    /// The refresh ADDS guards, it never rebuilds them: an op pending at pass
    /// start and replayed while the pass runs keeps its lock, because the
    /// server snapshot being reconciled may predate the replay.
    #[test]
    fn an_op_replayed_mid_pass_keeps_its_lock_for_the_rest_of_the_pass() {
        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor(&db, 9, 100);
        db.upsert_message(&descriptor_message(42)).unwrap();
        db.mark_read("acc:INBOX:42", true).unwrap();
        let mut provider = MidPassOp {
            db: &db,
            inner: QresyncMock {
                state: inbox_state(9, 100, 1),
                snapshot: Some(vec![uid_flags(42, false, false)]),
                ..Default::default()
            },
            hook: Some(replay_pending_ops),
        };
        run_mid_pass(&db, &mut provider);
        assert!(db.pending_operations("acc").unwrap().is_empty());
        assert!(is_read(&db, "acc:INBOX:42"));
    }

    #[test]
    fn a_local_change_landing_mid_sync_is_not_overwritten_by_the_final_cursor_write() {
        let db = temp_db();
        seed_account_mailbox(&db);
        db.set_account_write_enabled("acc", false).unwrap();
        seed_inbox_cursor_with_modseq(&db, 9, 100, 500);
        db.upsert_message(&descriptor_message(42)).unwrap();
        let state = inbox_state(9, 100, 1);
        let mut provider = MidRunLocalChange {
            db: &db,
            inner: QresyncMock {
                qresync_available: true,
                state,
                outcome: Some(qresync_delta(state, 600, vec![], vec![])),
                ..Default::default()
            },
        };
        sync_with_provider(
            &db,
            "acc",
            "test@local.test",
            None,
            &mut provider,
            "test-host",
        )
        .unwrap();
        assert_eq!(provider.inner.select_qresyncs, 1);
        assert!(
            !inbox_modseq(&db).is_some_and(|m| m > 0),
            "the mid-run invalidation must survive the sync's own cursor write, got {:?}",
            inbox_modseq(&db)
        );
    }

    fn descriptor_message(uid: i64) -> NewMessage {
        NewMessage {
            id: format!("acc:INBOX:{uid}"),
            account_id: "acc".into(),
            mailbox_id: "acc:INBOX".into(),
            provider_uid: Some(uid),
            rfc822_message_id: Some(format!("<d{uid}@example.com>")),
            subject: Some(format!("Descriptor {uid}")),
            from_name: Some("Alice".into()),
            from_address: Some("alice@example.com".into()),
            sent_at: Some(1_788_000_000 + uid),
            snippet: None,
            size_bytes: Some(10_000 + uid),
            has_attachments: true,
            attachments: None,
            is_read: false,
            is_starred: false,
            addresses: vec![Address {
                kind: "from".into(),
                name: Some("Alice".into()),
                address: "alice@example.com".into(),
            }],
            text_body: None,
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: vec![],
        }
    }

    fn remote_descriptor(uid: i64, subject: &str) -> RemoteMessageDescriptor {
        RemoteMessageDescriptor {
            keywords: None,
            uid,
            size_bytes: Some(10_000 + uid),
            has_attachments: Some(false),
            is_seen: false,
            is_flagged: false,
            rfc822_message_id: Some(format!("<d{uid}@example.com>")),
            subject: Some(subject.into()),
            from: Some(RemoteAddress {
                name: Some("Alice".into()),
                address: "alice@example.com".into(),
            }),
            to: vec![],
            cc: vec![],
            bcc: vec![],
            sent_at: Some(1_788_000_000 + uid),
            references: vec![],
        }
    }

    #[test]
    fn remote_flagged_state_lands_as_starred() {
        let db = temp_db();
        let mut mock = MockProvider {
            state: MailboxState {
                uid_validity: 1,
                uid_next: 3,
                exists: 2,
            },
            messages: vec![
                RemoteMessage {
                    is_flagged: true,
                    ..msg(1)
                },
                msg(2),
            ],
        };
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();

        let starred = db.get_message_detail("acc:INBOX:1").unwrap().unwrap();
        assert!(starred.header.is_starred, "\\Flagged must sync as starred");
        let plain = db.get_message_detail("acc:INBOX:2").unwrap().unwrap();
        assert!(!plain.header.is_starred);
    }

    #[test]
    fn sync_reconciles_a_vanished_server_draft_with_no_newer_candidate() {
        // S36: a locally-tracked server draft whose UID is no longer present
        // on the server (and nothing newer replaces it in Drafts) must have
        // its server pointer cleared by the reconciliation pass that runs at
        // the end of every ordinary sync — exercised end-to-end through
        // `sync_with_provider` rather than calling
        // `reconcile_server_draft_conflicts` directly (castellum-db's own
        // tests already cover that unit).
        use castellum_models::NewDraft;

        struct DraftsOnlyMock;
        impl MailProvider for DraftsOnlyMock {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![RemoteMailbox {
                    path: "Drafts".to_string(),
                    name: "Drafts".to_string(),
                    role: Some("drafts".to_string()),
                    role_source: Some(MailboxRoleSource::SpecialUse),
                    delimiter: Some("/".to_string()),
                    subscribed: true,
                    selectable: true,
                }])
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
                _after_uid: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                Ok(vec![]) // the tracked uid (and everything else) is gone
            }
            fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
                Ok(vec![])
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }

        let db = temp_db();
        db.upsert_account(&Account {
            id: "acc".into(),
            email: "test@local.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        let id = db
            .save_draft(&NewDraft {
                id: None,
                account_id: "acc".into(),
                to: vec![],
                cc: vec![],
                bcc: vec![],
                subject: "Mine".into(),
                body_text: "".into(),
                body_html: None,
                from_email: None,
                from_name: None,
                in_reply_to: None,
                references_header: None,
            })
            .unwrap();
        // We think we saved uid 5 to Drafts; the mock reports Drafts as empty.
        db.record_server_draft_saved(id, "acc", "Drafts", Some(1), Some(5), "<mine@x>")
            .unwrap();

        let mut mock = DraftsOnlyMock;
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();

        assert!(
            db.get_draft(id).unwrap().unwrap().server_uid.is_none(),
            "a vanished server draft with no newer candidate must have its pointer cleared by sync"
        );
    }

    #[test]
    fn sync_enqueues_background_embed_jobs_without_ever_running_them() {
        // S52: landing a message body during sync must queue an
        // `embed_message` job at background priority and return promptly.
        // `sync.rs` never imports `castellum_providers::AiBackend` at all
        // (see this file's top-level `use` block) — there is structurally no
        // code path here that could call out to an embedding backend, so
        // this proves the queued (not executed) state rather than racing a
        // real HTTP call.
        let db = temp_db();
        let mut mock = MockProvider {
            state: MailboxState {
                uid_validity: 1,
                uid_next: 3,
                exists: 2,
            },
            messages: vec![msg(1), msg(2)],
        };

        let started = std::time::Instant::now();
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "sync must return promptly — it never waits on the embedding queue"
        );

        let jobs = db.list_ai_jobs(10).unwrap();
        let embed_jobs: Vec<_> = jobs
            .iter()
            .filter(|j| j.feature == "embed_message")
            .collect();
        assert_eq!(embed_jobs.len(), 2, "one embed job per synced message body");
        for job in &embed_jobs {
            assert_eq!(job.status, "queued", "queued, not executed, by sync itself");
            assert_eq!(job.priority, castellum_db::AI_PRIORITY_BACKGROUND);
            let payload: serde_json::Value = serde_json::from_str(&job.payload_json).unwrap();
            assert_eq!(
                payload.as_object().unwrap().keys().collect::<Vec<_>>(),
                vec!["message_id"],
                "embed_message payload carries message_id only"
            );
        }
    }

    #[test]
    fn first_sync_fetches_all_then_incremental_fetches_only_new() {
        let db = temp_db();
        let mut mock = MockProvider {
            state: MailboxState {
                uid_validity: 1,
                uid_next: 3,
                exists: 2,
            },
            messages: vec![msg(1), msg(2)],
        };

        let r1 = sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
            .unwrap();
        assert_eq!(r1.mailboxes[0].fetched, 2);
        assert_eq!(
            r1.mailboxes[0].bytes_fetched,
            (raw_email(1).len() + raw_email(2).len()) as i64
        );
        assert_eq!(r1.mailboxes[0].fetched_uid_min, Some(1));
        assert_eq!(r1.mailboxes[0].fetched_uid_max, Some(2));
        assert_eq!(r1.mailboxes[0].total, 2);

        // A new message arrives; uid_next advances.
        mock.messages.push(msg(3));
        mock.state.uid_next = 4;
        mock.state.exists = 3;
        let r2 = sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
            .unwrap();
        assert_eq!(
            r2.mailboxes[0].fetched, 1,
            "only the new message is fetched"
        );
        assert_eq!(r2.mailboxes[0].bytes_fetched, raw_email(3).len() as i64);
        assert_eq!(r2.mailboxes[0].fetched_uid_min, Some(3));
        assert_eq!(r2.mailboxes[0].fetched_uid_max, Some(3));
        assert_eq!(r2.mailboxes[0].total, 3);

        // Parsed content landed.
        let detail = db.get_message_detail("acc:INBOX:1").unwrap().unwrap();
        assert_eq!(detail.header.subject.as_deref(), Some("Message 1"));
        assert_eq!(
            detail.header.from_address.as_deref(),
            Some("alice@example.com")
        );
    }

    #[test]
    fn descriptor_sync_imports_headers_without_fetching_bodies() {
        struct DescriptorMock {
            state: MailboxState,
            descriptors: Vec<RemoteMessageDescriptor>,
            body_fetch_called: bool,
        }

        impl MailProvider for DescriptorMock {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![rmb(
                    "INBOX",
                    "INBOX",
                    Some("inbox"),
                    Some("/"),
                    true,
                    true,
                )])
            }
            fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
                Ok(self.state)
            }
            fn fetch_new(
                &mut self,
                _after_uid: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                self.body_fetch_called = true;
                Err(ProviderError::Protocol("body fetch should not run".into()))
            }
            fn fetch_new_descriptors(
                &mut self,
                after_uid: Option<i64>,
            ) -> Result<Vec<RemoteMessageDescriptor>, ProviderError> {
                Ok(self
                    .descriptors
                    .iter()
                    .filter(|d| after_uid.is_none_or(|uid| d.uid > uid))
                    .cloned()
                    .collect())
            }
            fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
                Ok(self
                    .descriptors
                    .iter()
                    .map(|d| UidFlags {
                        keywords: None,
                        uid: d.uid,
                        is_seen: d.is_seen,
                        is_flagged: d.is_flagged,
                    })
                    .collect())
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }

        let db = temp_db();
        let mut mock = DescriptorMock {
            state: MailboxState {
                uid_validity: 1,
                uid_next: 3,
                exists: 2,
            },
            descriptors: vec![
                RemoteMessageDescriptor {
                    keywords: None,
                    uid: 1,
                    size_bytes: Some(42_000),
                    has_attachments: Some(true),
                    is_seen: true,
                    is_flagged: false,
                    rfc822_message_id: Some("<d1@example.com>".into()),
                    subject: Some("Descriptor one".into()),
                    from: Some(RemoteAddress {
                        name: Some("Alice".into()),
                        address: "alice@example.com".into(),
                    }),
                    to: vec![RemoteAddress {
                        name: None,
                        address: "test@local.test".into(),
                    }],
                    cc: vec![],
                    bcc: vec![],
                    sent_at: Some(1_788_000_000),
                    references: vec![],
                },
                RemoteMessageDescriptor {
                    keywords: None,
                    uid: 2,
                    size_bytes: Some(99_000),
                    has_attachments: None,
                    is_seen: false,
                    is_flagged: true,
                    rfc822_message_id: Some("<d2@example.com>".into()),
                    subject: Some("Descriptor two".into()),
                    from: Some(RemoteAddress {
                        name: None,
                        address: "bob@example.com".into(),
                    }),
                    to: vec![],
                    cc: vec![],
                    bcc: vec![],
                    sent_at: Some(1_788_000_060),
                    references: vec!["<d1@example.com>".into()],
                },
            ],
            body_fetch_called: false,
        };

        let report =
            sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
                .unwrap();
        assert!(
            !mock.body_fetch_called,
            "descriptor path must not fetch bodies"
        );
        assert_eq!(report.mailboxes[0].fetched, 2);
        assert_eq!(report.mailboxes[0].descriptors_fetched, 2);
        assert_eq!(report.mailboxes[0].bodies_fetched, 0);
        assert_eq!(report.mailboxes[0].bytes_fetched, 0);
        assert_eq!(report.mailboxes[0].fetched_uid_min, Some(1));
        assert_eq!(report.mailboxes[0].fetched_uid_max, Some(2));

        let detail = db.get_message_detail("acc:INBOX:1").unwrap().unwrap();
        assert_eq!(detail.header.subject.as_deref(), Some("Descriptor one"));
        assert_eq!(detail.header.from_name.as_deref(), Some("Alice"));
        assert_eq!(
            detail.header.from_address.as_deref(),
            Some("alice@example.com")
        );
        assert_eq!(detail.header.sent_at, Some(1_788_000_000));
        assert!(detail.header.has_attachments);
        assert!(!detail.header.has_body);
        assert!(detail.header.is_read);
        assert!(!detail.header.is_starred);
        assert_eq!(detail.text_body, None);
        assert_eq!(detail.sanitized_html, None);
    }

    // --- chunked descriptor fetch --------------------------

    /// A `MailProvider` whose descriptors are spread across several
    /// `DESCRIPTOR_CHUNK_SIZE`-sized UID ranges, so a mailbox with `uid_next`
    /// far above `DESCRIPTOR_CHUNK_SIZE` drives multiple `fetch_descriptor_
    /// chunk` calls per sync — records every `(low, high)` it was called
    /// with, and (via `fail_on_call`/`stop_on_call`) can simulate a real
    /// interruption or a budget stop on a specific call so the resume tests
    /// can prove the checkpoint works. Every listed mailbox serves the same
    /// descriptors (their ids are namespaced by mailbox, so no collision).
    struct ChunkedMock {
        state: MailboxState,
        descriptors: Vec<RemoteMessageDescriptor>,
        chunk_calls: Vec<(i64, i64)>,
        /// 1-based index into `chunk_calls`: that call fails instead of
        /// succeeding, simulating a network interruption mid-import.
        fail_on_call: Option<usize>,
        /// 1-based index into `chunk_calls`: that call stops early the way
        /// `ImapProvider` does on a budget stop — only UIDs at or below
        /// `covered_through` come back, with this `incomplete` attached. A
        /// `SessionAbandoned` stop also abandons the mock's session.
        stop_on_call: Option<(usize, DescriptorFetchIncomplete)>,
        /// UIDs every fetch reports as undecodable (skipped), whether in
        /// the import or a retry.
        undecodable: Vec<i64>,
        /// `sync_progress::snapshot`'s `fetched` count, captured at the START
        /// of each `fetch_descriptor_chunk` call — proves progress advances
        /// chunk by chunk rather than jumping straight from 0 to the total.
        progress_seen: Vec<i64>,
        /// Every `retry_descriptor_skips` call's UIDs.
        retry_calls: Vec<Vec<i64>>,
        /// Every retry call stops early like this (only UIDs at or below
        /// `covered_through` attempted); `SessionAbandoned` also abandons.
        retry_stop: Option<DescriptorFetchIncomplete>,
        /// "select:<path>", "chunk" and "retry", in call order.
        events: Vec<String>,
        mailboxes: Vec<&'static str>,
        abandoned: bool,
        account_id: &'static str,
    }

    impl ChunkedMock {
        fn new(
            state: MailboxState,
            descriptors: Vec<RemoteMessageDescriptor>,
            account_id: &'static str,
        ) -> Self {
            ChunkedMock {
                state,
                descriptors,
                chunk_calls: Vec::new(),
                fail_on_call: None,
                stop_on_call: None,
                undecodable: Vec::new(),
                progress_seen: Vec::new(),
                retry_calls: Vec::new(),
                retry_stop: None,
                events: Vec::new(),
                mailboxes: vec!["INBOX"],
                abandoned: false,
                account_id,
            }
        }
    }

    impl MailProvider for ChunkedMock {
        fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
            Ok(self
                .mailboxes
                .iter()
                .map(|&path| {
                    let role = (path == "INBOX").then_some("inbox");
                    rmb(path, path, role, Some("/"), true, true)
                })
                .collect())
        }
        fn select(&mut self, path: &str) -> Result<MailboxState, ProviderError> {
            self.events.push(format!("select:{path}"));
            if self.abandoned {
                return Err(ProviderError::Protocol("abandoned session reused".into()));
            }
            Ok(self.state)
        }
        fn fetch_new(
            &mut self,
            _after_uid: Option<i64>,
        ) -> Result<Vec<RemoteMessage>, ProviderError> {
            Err(ProviderError::Protocol(
                "legacy full-body fetch must not run when chunking succeeds".into(),
            ))
        }
        fn fetch_new_descriptors(
            &mut self,
            _after_uid: Option<i64>,
        ) -> Result<Vec<RemoteMessageDescriptor>, ProviderError> {
            Err(ProviderError::Protocol(
                "unbounded descriptor fetch must not run when the provider supports chunking"
                    .into(),
            ))
        }
        fn fetch_descriptor_chunk(
            &mut self,
            _mailbox_path: &str,
            low: i64,
            high: i64,
        ) -> Result<DescriptorChunkResult, ProviderError> {
            self.events.push("chunk".into());
            self.progress_seen.push(
                sync_progress::snapshot(self.account_id)
                    .and_then(|p| p.current_mailbox)
                    .map(|m| m.fetched)
                    .unwrap_or(0),
            );
            self.chunk_calls.push((low, high));
            if self.fail_on_call == Some(self.chunk_calls.len()) {
                return Err(ProviderError::Network("simulated interruption".into()));
            }
            let (top, incomplete) = match self.stop_on_call {
                Some((call, incomplete)) if call == self.chunk_calls.len() => (
                    incomplete.covered_through.unwrap_or(low - 1),
                    Some(incomplete),
                ),
                _ => (high, None),
            };
            if incomplete.is_some_and(|i| i.reason == DescriptorFetchStop::SessionAbandoned) {
                self.abandoned = true;
            }
            let in_range = |uid: i64| uid >= low && uid <= top;
            Ok(DescriptorChunkResult {
                descriptors: self
                    .descriptors
                    .iter()
                    .filter(|d| in_range(d.uid) && !self.undecodable.contains(&d.uid))
                    .cloned()
                    .collect(),
                skipped: self
                    .undecodable
                    .iter()
                    .filter(|&&uid| in_range(uid))
                    .map(|&uid| castellum_providers::SkippedDescriptor {
                        uid,
                        reason: castellum_providers::DescriptorSkipReason::Undecodable,
                    })
                    .collect(),
                incomplete,
            })
        }
        fn retry_descriptor_skips(
            &mut self,
            _mailbox_path: &str,
            uids: &[i64],
        ) -> Result<DescriptorChunkResult, ProviderError> {
            self.events.push("retry".into());
            self.retry_calls.push(uids.to_vec());
            let attempted: Vec<i64> = match self.retry_stop {
                Some(stop) => uids
                    .iter()
                    .copied()
                    .filter(|uid| stop.covered_through.is_some_and(|c| *uid <= c))
                    .collect(),
                None => uids.to_vec(),
            };
            if self
                .retry_stop
                .is_some_and(|s| s.reason == DescriptorFetchStop::SessionAbandoned)
            {
                self.abandoned = true;
            }
            Ok(DescriptorChunkResult {
                descriptors: self
                    .descriptors
                    .iter()
                    .filter(|d| attempted.contains(&d.uid) && !self.undecodable.contains(&d.uid))
                    .cloned()
                    .collect(),
                skipped: attempted
                    .iter()
                    .filter(|uid| self.undecodable.contains(uid))
                    .map(|&uid| castellum_providers::SkippedDescriptor {
                        uid,
                        reason: castellum_providers::DescriptorSkipReason::Undecodable,
                    })
                    .collect(),
                incomplete: self.retry_stop,
            })
        }
        fn session_abandoned(&self) -> bool {
            self.abandoned
        }
        fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
            Ok(self
                .descriptors
                .iter()
                .map(|d| UidFlags {
                    uid: d.uid,
                    is_seen: d.is_seen,
                    is_flagged: d.is_flagged,
                    keywords: None,
                })
                .collect())
        }
        fn logout(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    /// A provider that supports the sparse search+list path (the tracker
    /// #2501, Opus review point 2) but NOT `fetch_descriptor_chunk` — a
    /// mailbox this sparse must never fall back to walking the dense UID
    /// range, so if the sync engine's branching logic ever regressed to
    /// trying that instead, this mock would surface it loudly (an
    /// `Unsupported` provider error) rather than silently passing.
    struct SparseMock {
        state: MailboxState,
        descriptors: Vec<RemoteMessageDescriptor>,
        search_calls: Vec<(i64, i64)>,
        list_calls: Vec<Vec<i64>>,
        /// UIDs `search_uids_in` returns even though they are outside the
        /// requested span — a provider (or server) that does not filter
        /// RFC 3501's `n:*` last-UID quirk.
        out_of_span: Vec<i64>,
    }

    impl SparseMock {
        fn new(state: MailboxState, descriptors: Vec<RemoteMessageDescriptor>) -> Self {
            SparseMock {
                state,
                descriptors,
                search_calls: Vec::new(),
                list_calls: Vec::new(),
                out_of_span: Vec::new(),
            }
        }
    }

    impl MailProvider for SparseMock {
        fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
            Ok(vec![rmb(
                "INBOX",
                "INBOX",
                Some("inbox"),
                Some("/"),
                true,
                true,
            )])
        }
        fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
            Ok(self.state)
        }
        fn fetch_new(
            &mut self,
            _after_uid: Option<i64>,
        ) -> Result<Vec<RemoteMessage>, ProviderError> {
            Err(ProviderError::Protocol(
                "legacy full-body fetch must not run".into(),
            ))
        }
        fn search_uids_in(&mut self, low: i64, high: i64) -> Result<Vec<i64>, ProviderError> {
            self.search_calls.push((low, high));
            let mut uids: Vec<i64> = self
                .descriptors
                .iter()
                .map(|d| d.uid)
                .filter(|&uid| uid >= low && uid <= high)
                .collect();
            uids.extend(&self.out_of_span);
            Ok(uids)
        }
        fn fetch_descriptor_list(
            &mut self,
            _mailbox_path: &str,
            uids: &[i64],
        ) -> Result<DescriptorChunkResult, ProviderError> {
            self.list_calls.push(uids.to_vec());
            let wanted: std::collections::HashSet<i64> = uids.iter().copied().collect();
            Ok(DescriptorChunkResult {
                descriptors: self
                    .descriptors
                    .iter()
                    .filter(|d| wanted.contains(&d.uid))
                    .cloned()
                    .collect(),
                ..Default::default()
            })
        }
        fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
            Ok(self
                .descriptors
                .iter()
                .map(|d| UidFlags {
                    uid: d.uid,
                    is_seen: d.is_seen,
                    is_flagged: d.is_flagged,
                    keywords: None,
                })
                .collect())
        }
        fn logout(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    #[test]
    fn a_sparse_mailbox_is_searched_once_then_fetched_by_uid_list_not_walked() {
        let db = temp_db();
        let account_id = "acc-sparse";
        // A huge UID span (2M) with only 4 real messages spread across it --
        // Opus review's "300-message Spam with UIDNEXT 2M" scenario.
        let uids = [500i64, 780, 1_500_000, 1_999_999];
        let mut mock = SparseMock::new(
            MailboxState {
                uid_validity: 1,
                uid_next: 2_000_000,
                exists: uids.len() as i64,
            },
            uids.iter()
                .map(|&uid| remote_descriptor(uid, "sparse"))
                .collect(),
        );

        let report = sync_with_provider(
            &db,
            account_id,
            "test@local.test",
            None,
            &mut mock,
            "test-host",
        )
        .unwrap();

        assert_eq!(
            mock.search_calls,
            vec![(1, 1_999_999)],
            "one UID SEARCH over the whole span, never a per-window search"
        );
        assert_eq!(
            mock.list_calls,
            vec![uids.to_vec()],
            "the 4 sparse UIDs fit in one group and are fetched by explicit \
             list, never as a 1:2000000-shaped dense range"
        );
        let mb = &report.mailboxes[0];
        assert!(mb.error.is_none(), "{mb:?}");
        assert_eq!(mb.fetched, 4);
        assert_eq!(
            db.count_messages(account_id, &format!("{account_id}:INBOX"))
                .unwrap(),
            4
        );
    }

    #[test]
    fn a_chunked_first_sync_persists_per_chunk_and_advances_progress_per_chunk() {
        let db = temp_db();
        let account_id = "acc-chunk-progress";
        let mut mock = ChunkedMock::new(
            MailboxState {
                uid_validity: 1,
                uid_next: 2501,
                exists: 3,
            },
            vec![
                remote_descriptor(500, "first chunk"),
                remote_descriptor(1500, "second chunk"),
                remote_descriptor(2500, "third chunk"),
            ],
            account_id,
        );

        let report = sync_with_provider(
            &db,
            account_id,
            "test@local.test",
            None,
            &mut mock,
            "test-host",
        )
        .unwrap();

        assert_eq!(
            mock.chunk_calls,
            vec![(1, 1000), (1001, 2000), (2001, 2500)],
            "three ~1000-UID chunks, lowest-first"
        );
        assert_eq!(
            mock.progress_seen,
            vec![0, 1, 2],
            "progress must advance after each committed chunk, not jump straight to the total"
        );

        let mb = &report.mailboxes[0];
        assert_eq!(mb.fetched, 3);
        assert_eq!(mb.descriptors_fetched, 3);
        assert_eq!(mb.descriptors_skipped, 0);
        assert_eq!(mb.fetched_uid_min, Some(500));
        assert_eq!(mb.fetched_uid_max, Some(2500));
        assert!(mb.error.is_none());

        // The completed pass clears the chunk checkpoint and writes the
        // ordinary cursor, exactly like the legacy unbounded path.
        let cursor = db
            .get_sync_cursor(account_id, &format!("{account_id}:INBOX"))
            .unwrap()
            .unwrap();
        assert_eq!(cursor.uid_validity, Some(1));
        assert_eq!(cursor.uid_next, Some(2501));
        assert_eq!(cursor.descriptor_checkpoint_uid, None);
    }

    #[test]
    fn an_interrupted_chunked_import_resumes_from_the_last_committed_chunk() {
        let db = temp_db();
        let account_id = "acc-chunk-resume";
        let descriptors = vec![
            remote_descriptor(500, "first chunk"),
            remote_descriptor(1500, "second chunk"),
            remote_descriptor(2500, "third chunk"),
        ];
        let state = MailboxState {
            uid_validity: 1,
            uid_next: 2501,
            exists: 3,
        };
        let mailbox_id = format!("{account_id}:INBOX");

        // Pass 1: the second chunk call ([1001, 2000]) fails outright, as a
        // real network interruption would.
        let mut mock1 = ChunkedMock {
            fail_on_call: Some(2),
            ..ChunkedMock::new(state, descriptors.clone(), account_id)
        };
        let report1 = sync_with_provider(
            &db,
            account_id,
            "test@local.test",
            None,
            &mut mock1,
            "test-host",
        )
        .unwrap();
        assert_eq!(mock1.chunk_calls, vec![(1, 1000), (1001, 2000)]);
        let mb1 = &report1.mailboxes[0];
        assert!(mb1.error.is_some(), "the interruption must be reported");
        assert_eq!(mb1.fetched, 1, "only the first chunk committed");
        assert_eq!(
            mb1.cancelled_ops, 0,
            "an interruption mid-import must never purge/cancel like a UIDVALIDITY reset"
        );
        assert_eq!(
            db.count_messages(account_id, &mailbox_id).unwrap(),
            1,
            "the first chunk's message survives the failed pass"
        );
        let cursor1 = db
            .get_sync_cursor(account_id, &mailbox_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            cursor1.uid_next, None,
            "the pass never finished, so the ordinary cursor is not written yet"
        );
        assert_eq!(
            cursor1.descriptor_checkpoint_uid,
            Some(1000),
            "checkpointed at the end of the last COMMITTED chunk"
        );
        assert_eq!(cursor1.uid_validity, Some(1));

        // Pass 2, same account/db, a fresh (non-failing) mock: must resume
        // at 1001, never re-fetch [1, 1000], and never purge the row pass 1
        // already committed (/#175's purge path must not fire
        // just because `uid_next` is still unset).
        let mut mock2 = ChunkedMock::new(state, descriptors, account_id);
        let report2 = sync_with_provider(
            &db,
            account_id,
            "test@local.test",
            None,
            &mut mock2,
            "test-host",
        )
        .unwrap();
        assert_eq!(
            mock2.chunk_calls,
            vec![(1001, 2000), (2001, 2500)],
            "resumes right after the last committed chunk, never from UID 1"
        );
        let mb2 = &report2.mailboxes[0];
        assert!(mb2.error.is_none());
        assert_eq!(
            mb2.fetched, 2,
            "only the two chunks resume actually fetched"
        );
        assert_eq!(
            mb2.cancelled_ops, 0,
            "resuming a chunked import must not take the purge-and-refetch path"
        );
        assert_eq!(
            db.count_messages(account_id, &mailbox_id).unwrap(),
            3,
            "all three messages present: none purged, none duplicated"
        );
        let cursor2 = db
            .get_sync_cursor(account_id, &mailbox_id)
            .unwrap()
            .unwrap();
        assert_eq!(cursor2.uid_next, Some(2501), "the pass completed this time");
        assert_eq!(
            cursor2.descriptor_checkpoint_uid, None,
            "checkpoint cleared"
        );
    }

    #[test]
    fn an_interrupted_reimport_after_a_uidvalidity_reset_resumes_rather_than_purging_again() {
        // Opus review of PR #180, point 3: a UIDVALIDITY reset used to purge
        // the mailbox's rows but leave the cursor row's `uid_validity` at
        // the OLD value, so a chunked import interrupted mid-resume would
        // find `same_uid_space` false again on the next pass and purge
        // (and restart from UID 1) forever.
        let db = temp_db();
        let account_id = "acc-reset-resume";
        let mailbox_id = format!("{account_id}:INBOX");

        db.upsert_account(&Account {
            id: account_id.to_string(),
            email: "test@local.test".to_string(),
            display_name: None,
            provider: "imap".to_string(),
        })
        .unwrap();
        db.upsert_mailbox(&Mailbox {
            id: mailbox_id.clone(),
            account_id: account_id.to_string(),
            path: "INBOX".to_string(),
            name: "INBOX".to_string(),
            role: Some("inbox".to_string()),
            delimiter: Some("/".to_string()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        // A message and cursor from an EARLIER sync, in the OLD (V1) UID
        // space -- the message must be purged once the reset is detected.
        db.upsert_messages_batch(&[new_message_from_descriptor(
            account_id,
            &mailbox_id,
            remote_descriptor(999, "stale V1 message"),
        )])
        .unwrap();
        db.set_sync_cursor(
            account_id,
            &mailbox_id,
            &SyncCursor {
                uid_validity: Some(1),
                uid_next: Some(1000),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();

        let descriptors = vec![
            remote_descriptor(500, "first chunk"),
            remote_descriptor(1500, "second chunk"),
            remote_descriptor(2500, "third chunk"),
        ];
        // The server reset to V2 -- a different uid_validity than the
        // cursor's V1.
        let state = MailboxState {
            uid_validity: 2,
            uid_next: 2501,
            exists: 3,
        };

        let mut mock1 = ChunkedMock {
            fail_on_call: Some(2),
            ..ChunkedMock::new(state, descriptors.clone(), account_id)
        };
        let report1 = sync_with_provider(
            &db,
            account_id,
            "test@local.test",
            None,
            &mut mock1,
            "test-host",
        )
        .unwrap();
        let mb1 = &report1.mailboxes[0];
        assert!(mb1.error.is_some(), "the interruption must be reported");
        assert_eq!(
            db.count_messages(account_id, &mailbox_id).unwrap(),
            1,
            "the stale V1 message is purged; the first V2 chunk's message survives"
        );
        let cursor1 = db
            .get_sync_cursor(account_id, &mailbox_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            cursor1.uid_validity,
            Some(2),
            "the purge resets the cursor to the NEW uid_validity, in the SAME \
             transaction — this is the fix"
        );
        assert_eq!(cursor1.uid_next, None);
        assert_eq!(cursor1.descriptor_checkpoint_uid, Some(1000));

        // Pass 2: same_uid_space is now TRUE (cursor.uid_validity == 2 ==
        // state.uid_validity), so this must resume past the checkpoint
        // instead of purging (and restarting at UID 1) again.
        let mut mock2 = ChunkedMock::new(state, descriptors, account_id);
        let report2 = sync_with_provider(
            &db,
            account_id,
            "test@local.test",
            None,
            &mut mock2,
            "test-host",
        )
        .unwrap();
        assert_eq!(
            mock2.chunk_calls,
            vec![(1001, 2000), (2001, 2500)],
            "resumes right after the checkpoint, never re-purges and restarts at UID 1"
        );
        let mb2 = &report2.mailboxes[0];
        assert!(mb2.error.is_none());
        assert_eq!(
            mb2.cancelled_ops, 0,
            "same_uid_space is true this pass: no second purge"
        );
        assert_eq!(db.count_messages(account_id, &mailbox_id).unwrap(), 3);
        let cursor2 = db
            .get_sync_cursor(account_id, &mailbox_id)
            .unwrap()
            .unwrap();
        assert_eq!(cursor2.uid_validity, Some(2));
        assert_eq!(cursor2.uid_next, Some(2501));
        assert_eq!(cursor2.descriptor_checkpoint_uid, None);
    }

    /// `now_epoch()` minus two hours: a skip recorded then is already due
    /// (a fresh skip waits one hour).
    fn two_hours_ago() -> i64 {
        now_epoch() - 2 * 3600
    }

    const SKIP_REASON: &str = "message could not be decoded and was skipped";

    fn seed_synced_mailbox(db: &Database, account_id: &str, path: &str, uid_next: i64) {
        let mailbox_id = format!("{account_id}:{path}");
        db.upsert_account(&Account {
            id: account_id.to_string(),
            email: "test@local.test".to_string(),
            display_name: None,
            provider: "imap".to_string(),
        })
        .unwrap();
        db.upsert_mailbox(&Mailbox {
            id: mailbox_id.clone(),
            account_id: account_id.to_string(),
            path: path.to_string(),
            name: path.to_string(),
            role: (path == "INBOX").then(|| "inbox".to_string()),
            delimiter: Some("/".to_string()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.set_sync_cursor(
            account_id,
            &mailbox_id,
            &SyncCursor {
                uid_validity: Some(5),
                uid_next: Some(uid_next),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
    }

    fn state5(uid_next: i64) -> MailboxState {
        MailboxState {
            uid_validity: 5,
            uid_next,
            exists: 1,
        }
    }

    #[test]
    fn the_retry_set_recovers_a_previously_skipped_uid_and_clears_the_row() {
        // Opus review of PR #180, point 1(c): a UID a chunked fetch isolated
        // as undecodable must not be lost forever just because the
        // checkpoint/uid_next moved past it -- once its backoff expires the
        // retry pass re-fetches it and, once it decodes, both persists the
        // message and removes the retry-set row.
        let db = temp_db();
        let account_id = "acc-retry-set";
        let mailbox_id = format!("{account_id}:INBOX");
        // A mailbox already fully synced through UID 99, with UID 42
        // sitting in the retry set from an earlier pass's isolated skip.
        seed_synced_mailbox(&db, account_id, "INBOX", 100);
        db.record_message_decode_skip(account_id, &mailbox_id, 42, 5, SKIP_REASON, two_hours_ago())
            .unwrap();

        // UID 42 now decodes fine (the mock stands in for a decoder fix, or
        // it was a one-off after all).
        let mut mock = ChunkedMock::new(
            state5(100),
            vec![remote_descriptor(42, "recovered")],
            account_id,
        );
        let report = sync_with_provider(
            &db,
            account_id,
            "test@local.test",
            None,
            &mut mock,
            "test-host",
        )
        .unwrap();

        assert!(
            mock.chunk_calls.is_empty(),
            "the main range [100, 99] is empty"
        );
        assert_eq!(mock.retry_calls, vec![vec![42]]);
        assert!(report.mailboxes[0].error.is_none());
        assert_eq!(
            db.count_message_decode_skips(account_id, &mailbox_id)
                .unwrap(),
            0,
            "recovered: removed from the retry set"
        );
        let detail = db
            .get_message_detail(&format!("{mailbox_id}:42"))
            .unwrap()
            .unwrap();
        assert_eq!(detail.header.subject.as_deref(), Some("recovered"));
    }

    /// Merge of S42 (PR #173) over #180: the chunked descriptor import and the
    /// decode-skip retry persist through `upsert_messages_batch`, not S42's
    /// per-descriptor write, so both must still store the server keywords a
    /// descriptor carries. `ChunkedMock::list_uid_flags` reports keywords as
    /// `None`, so only the descriptor path can have written them.
    #[test]
    fn chunked_import_and_retry_store_descriptor_keywords() {
        let keywords_of = |db: &Database, id: &str| {
            db.keywords_for_messages(&[id.to_string()])
                .unwrap()
                .get(id)
                .cloned()
                .unwrap_or_default()
        };

        // First sync through fetch_descriptor_chunk.
        let db = temp_db();
        let account_id = "acc-chunk-keywords";
        let mut mock = ChunkedMock::new(
            MailboxState {
                uid_validity: 1,
                uid_next: 1501,
                exists: 2,
            },
            vec![
                RemoteMessageDescriptor {
                    keywords: Some(vec!["Work".to_string()]),
                    ..remote_descriptor(500, "tagged")
                },
                remote_descriptor(1500, "untagged"),
            ],
            account_id,
        );
        let report = sync_with_provider(
            &db,
            account_id,
            "test@local.test",
            None,
            &mut mock,
            "test-host",
        )
        .unwrap();
        assert!(report.mailboxes[0].error.is_none(), "{report:?}");
        assert!(!mock.chunk_calls.is_empty(), "the chunked path ran");
        assert_eq!(
            keywords_of(&db, &format!("{account_id}:INBOX:500")),
            vec!["Work".to_string()]
        );
        assert!(keywords_of(&db, &format!("{account_id}:INBOX:1500")).is_empty());

        // A UID recovered by the retry pass.
        let db = temp_db();
        let account_id = "acc-retry-keywords";
        let mailbox_id = format!("{account_id}:INBOX");
        seed_synced_mailbox(&db, account_id, "INBOX", 100);
        db.record_message_decode_skip(account_id, &mailbox_id, 42, 5, SKIP_REASON, two_hours_ago())
            .unwrap();
        let mut mock = ChunkedMock::new(
            state5(100),
            vec![RemoteMessageDescriptor {
                keywords: Some(vec!["$Label1".to_string()]),
                ..remote_descriptor(42, "recovered")
            }],
            account_id,
        );
        sync_with_provider(
            &db,
            account_id,
            "test@local.test",
            None,
            &mut mock,
            "test-host",
        )
        .unwrap();
        assert_eq!(mock.retry_calls, vec![vec![42]]);
        assert_eq!(
            keywords_of(&db, &format!("{mailbox_id}:42")),
            vec!["$Label1".to_string()]
        );
    }

    /// Review of PR #180, round 2, finding 3: retries ran BEFORE the import,
    /// on the import's reconnect budget, every pass. They now run after it,
    /// only for rows whose backoff has expired.
    #[test]
    fn retries_run_after_the_import_and_only_once_their_backoff_expires() {
        let db = temp_db();
        let account_id = "acc-retry-order";
        let mailbox_id = format!("{account_id}:INBOX");
        seed_synced_mailbox(&db, account_id, "INBOX", 50);
        db.record_message_decode_skip(account_id, &mailbox_id, 42, 5, SKIP_REASON, two_hours_ago())
            .unwrap();
        // Recorded just now: not due for another hour.
        db.record_message_decode_skip(account_id, &mailbox_id, 43, 5, SKIP_REASON, now_epoch())
            .unwrap();

        let mut mock = ChunkedMock::new(
            state5(100),
            vec![
                remote_descriptor(42, "recovered"),
                remote_descriptor(60, "new"),
            ],
            account_id,
        );
        sync_with_provider(&db, account_id, "t@l.test", None, &mut mock, "h").unwrap();

        assert_eq!(mock.chunk_calls, vec![(50, 99)]);
        assert_eq!(
            mock.events,
            vec!["select:INBOX", "chunk", "retry"],
            "the import runs first; retries never compete with it"
        );
        assert_eq!(mock.retry_calls, vec![vec![42]], "43 is not due yet");
        assert_eq!(
            db.message_decode_skip_backoff(account_id, &mailbox_id, 5, 43)
                .unwrap()
                .map(|(attempts, _)| attempts),
            Some(0),
            "an undue row is left untouched"
        );
        assert_eq!(
            db.message_decode_skip_backoff(account_id, &mailbox_id, 5, 42)
                .unwrap(),
            None
        );
        assert_eq!(db.count_messages(account_id, &mailbox_id).unwrap(), 2);
    }

    /// Finding 3: a permanently bad UID used to cost two LOGINs on every
    /// pass, forever. A failed retry now backs off (1h, 4h, 24h, then 7d),
    /// and until then it is not attempted at all — zero reconnects.
    #[test]
    fn a_permanently_bad_uid_backs_off_and_costs_nothing_until_it_is_due() {
        let db = temp_db();
        let account_id = "acc-retry-backoff";
        let mailbox_id = format!("{account_id}:INBOX");
        seed_synced_mailbox(&db, account_id, "INBOX", 100);
        db.record_message_decode_skip(account_id, &mailbox_id, 42, 5, SKIP_REASON, two_hours_ago())
            .unwrap();

        let mut pass1 = ChunkedMock {
            undecodable: vec![42],
            ..ChunkedMock::new(state5(100), vec![remote_descriptor(42, "bad")], account_id)
        };
        let before = now_epoch();
        let report =
            sync_with_provider(&db, account_id, "t@l.test", None, &mut pass1, "h").unwrap();
        assert_eq!(pass1.retry_calls, vec![vec![42]]);
        assert!(
            report.mailboxes[0].error.is_none(),
            "a failed retry never fails the mailbox"
        );
        let (attempts, next_retry_at) = db
            .message_decode_skip_backoff(account_id, &mailbox_id, 5, 42)
            .unwrap()
            .expect("still in the retry set");
        assert_eq!(attempts, 1);
        assert!(
            (before + 4 * 3600..=now_epoch() + 4 * 3600).contains(&next_retry_at),
            "next retry 4h out, got {next_retry_at} (now {before})"
        );

        let mut pass2 = ChunkedMock {
            undecodable: vec![42],
            ..ChunkedMock::new(state5(100), vec![remote_descriptor(42, "bad")], account_id)
        };
        sync_with_provider(&db, account_id, "t@l.test", None, &mut pass2, "h").unwrap();
        assert!(
            pass2.retry_calls.is_empty(),
            "not due: no retry, so no reconnect, at all"
        );
    }

    #[test]
    fn the_retry_pass_retries_at_most_20_uids_per_sync_pass() {
        let db = temp_db();
        let account_id = "acc-retry-cap";
        for path in ["INBOX", "Work"] {
            seed_synced_mailbox(&db, account_id, path, 100);
            for uid in 1..=15 {
                db.record_message_decode_skip(
                    account_id,
                    &format!("{account_id}:{path}"),
                    uid,
                    5,
                    SKIP_REASON,
                    two_hours_ago(),
                )
                .unwrap();
            }
        }
        let mut mock = ChunkedMock {
            mailboxes: vec!["INBOX", "Work"],
            undecodable: (1..=15).collect(),
            ..ChunkedMock::new(state5(100), vec![], account_id)
        };
        sync_with_provider(&db, account_id, "t@l.test", None, &mut mock, "h").unwrap();
        let retried: usize = mock.retry_calls.iter().map(Vec::len).sum();
        assert_eq!(retried, 20, "{:?}", mock.retry_calls);
        assert_eq!(mock.retry_calls[0].len(), 15);
        assert_eq!(mock.retry_calls[1].len(), 5);
    }

    /// Review round 3: when the provider's retry budget runs dry part-way,
    /// only the UIDs it actually attempted may change — an unattempted row
    /// must neither be deleted (that would lose the message for good) nor
    /// have its backoff bumped.
    #[test]
    fn a_retry_that_stops_early_leaves_unattempted_rows_untouched() {
        let db = temp_db();
        let account_id = "acc-retry-stop";
        let mailbox_id = format!("{account_id}:INBOX");
        seed_synced_mailbox(&db, account_id, "INBOX", 100);
        for (uid, age) in [(40, 30), (41, 20), (42, 10)] {
            db.record_message_decode_skip(
                account_id,
                &mailbox_id,
                uid,
                5,
                SKIP_REASON,
                two_hours_ago() - age,
            )
            .unwrap();
        }
        let before: Vec<_> = [41, 42]
            .iter()
            .map(|&uid| {
                db.message_decode_skip_backoff(account_id, &mailbox_id, 5, uid)
                    .unwrap()
            })
            .collect();

        let mut mock = ChunkedMock {
            undecodable: vec![40, 41, 42],
            retry_stop: Some(DescriptorFetchIncomplete {
                covered_through: Some(40),
                reason: DescriptorFetchStop::ReconnectBudgetExhausted,
            }),
            ..ChunkedMock::new(state5(100), vec![], account_id)
        };
        sync_with_provider(&db, account_id, "t@l.test", None, &mut mock, "h").unwrap();

        assert_eq!(mock.retry_calls, vec![vec![40, 41, 42]]);
        assert_eq!(
            db.message_decode_skip_backoff(account_id, &mailbox_id, 5, 40)
                .unwrap()
                .map(|(attempts, _)| attempts),
            Some(1),
            "attempted and still bad: backed off"
        );
        let after: Vec<_> = [41, 42]
            .iter()
            .map(|&uid| {
                db.message_decode_skip_backoff(account_id, &mailbox_id, 5, uid)
                    .unwrap()
            })
            .collect();
        assert_eq!(after, before, "never attempted: kept exactly as they were");
    }

    #[test]
    fn a_retry_that_abandons_the_session_ends_the_pass() {
        let db = temp_db();
        let account_id = "acc-retry-abandon";
        let mailbox_id = format!("{account_id}:INBOX");
        seed_synced_mailbox(&db, account_id, "INBOX", 100);
        seed_synced_mailbox(&db, account_id, "Work", 100);
        db.record_message_decode_skip(account_id, &mailbox_id, 40, 5, SKIP_REASON, two_hours_ago())
            .unwrap();
        let mut mock = ChunkedMock {
            mailboxes: vec!["INBOX", "Work"],
            undecodable: vec![40],
            retry_stop: Some(DescriptorFetchIncomplete {
                covered_through: Some(40),
                reason: DescriptorFetchStop::SessionAbandoned,
            }),
            ..ChunkedMock::new(state5(100), vec![], account_id)
        };
        let report = sync_with_provider(&db, account_id, "t@l.test", None, &mut mock, "h").unwrap();

        assert_eq!(report.mailboxes.len(), 1, "{:?}", report.mailboxes);
        assert!(report.mailboxes[0]
            .error
            .as_deref()
            .is_some_and(|e| e.contains("abandoned")));
        assert!(
            !mock.events.iter().any(|e| e == "select:Work"),
            "{:?}",
            mock.events
        );
        assert_eq!(
            db.message_decode_skip_backoff(account_id, &mailbox_id, 5, 40)
                .unwrap()
                .map(|(attempts, _)| attempts),
            Some(1)
        );
    }

    /// Review round 3, finding 2: a session poisoned by a call that is not
    /// descriptor isolation (here the body fetch) must end the pass BEFORE
    /// the next mailbox's SELECT, not after a bogus SELECT failure is
    /// recorded against that innocent mailbox.
    #[test]
    fn a_session_abandoned_mid_mailbox_is_never_used_for_the_next_select() {
        struct PoisonedByBodies {
            selects: Vec<String>,
            abandoned: bool,
        }
        impl MailProvider for PoisonedByBodies {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![
                    rmb("INBOX", "INBOX", Some("inbox"), Some("/"), true, true),
                    rmb("Work", "Work", None, Some("/"), true, true),
                ])
            }
            fn select(&mut self, path: &str) -> Result<MailboxState, ProviderError> {
                self.selects.push(path.to_string());
                Ok(MailboxState {
                    uid_validity: 1,
                    uid_next: 2,
                    exists: 1,
                })
            }
            fn fetch_new(&mut self, _: Option<i64>) -> Result<Vec<RemoteMessage>, ProviderError> {
                Ok(vec![])
            }
            fn fetch_new_descriptors(
                &mut self,
                _: Option<i64>,
            ) -> Result<Vec<RemoteMessageDescriptor>, ProviderError> {
                Ok(vec![remote_descriptor(1, "needs a body")])
            }
            fn fetch_bodies(&mut self, _: &[i64]) -> Result<Vec<RemoteMessage>, ProviderError> {
                self.abandoned = true;
                Err(ProviderError::Protocol("undecodable body response".into()))
            }
            fn session_abandoned(&self) -> bool {
                self.abandoned
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }

        let db = temp_db();
        let mut provider = PoisonedByBodies {
            selects: Vec::new(),
            abandoned: false,
        };
        let report = sync_with_provider(
            &db,
            "acc-poisoned-bodies",
            "t@l.test",
            None,
            &mut provider,
            "h",
        )
        .unwrap();
        assert_eq!(
            provider.selects,
            vec!["INBOX"],
            "Work must never be selected"
        );
        assert_eq!(report.mailboxes.len(), 1);
        assert!(db
            .list_mailbox_sync_states("acc-poisoned-bodies")
            .unwrap()
            .iter()
            .all(|s| s.mailbox_id.ends_with(":INBOX")));
    }

    /// Findings 1 + 2 at the sync layer: a provider that stops early on its
    /// reconnect budget returns what it covered — those descriptors and
    /// skips are committed and the checkpoint moves to exactly the covered
    /// point (never past an unrecorded UID). The mailbox reports a typed
    /// error, the NEXT mailbox still syncs, and the next pass resumes just
    /// past the checkpoint.
    #[test]
    fn a_budget_stop_commits_partial_progress_and_the_next_mailbox_still_syncs() {
        let db = temp_db();
        let account_id = "acc-partial";
        let inbox_id = format!("{account_id}:INBOX");
        let work_id = format!("{account_id}:Work");
        let descriptors = vec![
            remote_descriptor(100, "a"),
            remote_descriptor(200, "b"),
            remote_descriptor(300, "c"),
        ];
        let state = MailboxState {
            uid_validity: 1,
            uid_next: 1001,
            exists: 4,
        };
        let mut pass1 = ChunkedMock {
            mailboxes: vec!["INBOX", "Work"],
            undecodable: vec![150],
            stop_on_call: Some((
                1,
                DescriptorFetchIncomplete {
                    covered_through: Some(260),
                    reason: DescriptorFetchStop::ReconnectBudgetExhausted,
                },
            )),
            ..ChunkedMock::new(state, descriptors.clone(), account_id)
        };
        let report =
            sync_with_provider(&db, account_id, "t@l.test", None, &mut pass1, "h").unwrap();

        let inbox = &report.mailboxes[0];
        assert_eq!(inbox.path, "INBOX");
        assert!(
            inbox
                .error
                .as_deref()
                .is_some_and(|e| e.contains("reconnect budget")),
            "{inbox:?}"
        );
        assert_eq!(inbox.fetched, 2, "UIDs 100 and 200 committed");
        assert_eq!(inbox.descriptors_skipped, 1);
        assert_eq!(db.count_messages(account_id, &inbox_id).unwrap(), 2);
        assert_eq!(
            db.message_decode_skip_backoff(account_id, &inbox_id, 1, 150)
                .unwrap()
                .map(|(attempts, _)| attempts),
            Some(0),
            "the isolated skip is kept, not dropped with the error"
        );
        let cursor = db.get_sync_cursor(account_id, &inbox_id).unwrap().unwrap();
        assert_eq!(cursor.descriptor_checkpoint_uid, Some(260));
        assert_eq!(cursor.uid_next, None, "the mailbox did not finish");

        let work = &report.mailboxes[1];
        assert_eq!(work.path, "Work");
        assert!(work.error.is_none(), "{work:?}");
        assert_eq!(db.count_messages(account_id, &work_id).unwrap(), 3);

        let mut pass2 = ChunkedMock {
            mailboxes: vec!["INBOX", "Work"],
            undecodable: vec![150],
            ..ChunkedMock::new(state, descriptors, account_id)
        };
        sync_with_provider(&db, account_id, "t@l.test", None, &mut pass2, "h").unwrap();
        assert_eq!(
            pass2.chunk_calls[0],
            (261, 1000),
            "INBOX resumes just past the covered point"
        );
        assert_eq!(db.count_messages(account_id, &inbox_id).unwrap(), 3);
    }

    /// #181 x #180 (merge check): #181's incremental thread rebuild only
    /// revisits messages some write marked dirty. Both of this PR's write
    /// paths — a chunked import that stopped part-way, and a retry-set
    /// recovery that reports nothing as `fetched` — go through
    /// `upsert_messages_batch`, which marks every NEW row dirty in its own
    /// transaction, so the pass-end rebuild must thread them all and leave
    /// nothing dirty.
    #[test]
    fn chunked_and_retried_messages_are_threaded_by_the_incremental_rebuild() {
        let db = temp_db();
        let account_id = "acc-threads";
        let mut partial = ChunkedMock {
            undecodable: vec![150],
            stop_on_call: Some((
                1,
                DescriptorFetchIncomplete {
                    covered_through: Some(260),
                    reason: DescriptorFetchStop::ReconnectBudgetExhausted,
                },
            )),
            ..ChunkedMock::new(
                MailboxState {
                    uid_validity: 5,
                    uid_next: 1001,
                    exists: 3,
                },
                vec![
                    remote_descriptor(100, "a"),
                    remote_descriptor(200, "b"),
                    remote_descriptor(300, "c"),
                ],
                account_id,
            )
        };
        sync_with_provider(&db, account_id, "t@l.test", None, &mut partial, "h").unwrap();
        let thread_of = |uid: i64| {
            db.get_message_detail(&format!("{account_id}:INBOX:{uid}"))
                .unwrap()
                .and_then(|d| d.thread_id)
        };
        assert!(thread_of(100).is_some() && thread_of(200).is_some());
        assert!(!db.threads_dirty(account_id).unwrap());

        // A later pass whose ONLY new message comes from the retry set: its
        // report shows nothing fetched, so only the dirty marker can make
        // the pass-end rebuild run.
        // (150, skipped just now, is not due yet; 120 stands in for an
        // older skip whose backoff has expired.)
        let mailbox_id = format!("{account_id}:INBOX");
        db.record_message_decode_skip(
            account_id,
            &mailbox_id,
            120,
            5,
            SKIP_REASON,
            two_hours_ago(),
        )
        .unwrap();
        let mut retry = ChunkedMock::new(
            MailboxState {
                uid_validity: 5,
                uid_next: 1001,
                exists: 3,
            },
            vec![remote_descriptor(120, "recovered")],
            account_id,
        );
        let report =
            sync_with_provider(&db, account_id, "t@l.test", None, &mut retry, "h").unwrap();
        assert_eq!(
            report.mailboxes[0].fetched, 0,
            "a recovery is not counted as fetched"
        );
        assert_eq!(retry.retry_calls, vec![vec![120]]);
        assert!(
            thread_of(120).is_some(),
            "the recovered message was threaded"
        );
        assert!(!db.threads_dirty(account_id).unwrap());
    }

    /// Finding 2: once the provider abandons its session (a decode failure
    /// with no reconnect left), the pass ends there — no later mailbox is
    /// even selected — and the account reads failed, not healthy.
    #[test]
    fn an_abandoned_session_ends_the_pass_and_the_account_reads_failed() {
        let db = temp_db();
        let json = sync_imap_json_with(&db, sync_cfg("acc-abandon"), |_cfg| {
            Ok(ChunkedMock {
                mailboxes: vec!["INBOX", "Work"],
                stop_on_call: Some((
                    1,
                    DescriptorFetchIncomplete {
                        covered_through: None,
                        reason: DescriptorFetchStop::SessionAbandoned,
                    },
                )),
                ..ChunkedMock::new(
                    MailboxState {
                        uid_validity: 1,
                        uid_next: 11,
                        exists: 1,
                    },
                    vec![remote_descriptor(5, "x")],
                    "acc-abandon",
                )
            })
        })
        .expect("the pass itself still returns Ok");
        let report: serde_json::Value = serde_json::from_str(&json).unwrap();
        let mailboxes = report["mailboxes"].as_array().unwrap();
        assert_eq!(
            mailboxes.len(),
            1,
            "Work is never attempted on an abandoned session: {mailboxes:?}"
        );
        assert!(mailboxes[0]["error"]
            .as_str()
            .is_some_and(|e| e.contains("abandoned")));

        let state = db.get_account_sync_state("acc-abandon").unwrap().unwrap();
        assert!(state.last_success_at.is_none(), "not recorded as a success");
        assert_eq!(state.consecutive_failures, 1);
        assert_eq!(state.last_error_phase.as_deref(), Some("sync"));
        let mailbox_states = db.list_mailbox_sync_states("acc-abandon").unwrap();
        assert_eq!(mailbox_states.len(), 1, "{mailbox_states:?}");
    }

    /// Merge of main into S31 (2026-09-26): S31 moved LOGOUT out of the
    /// shared `record_and_run_sync` (an IDLE resync keeps its connection),
    /// so the session-abandoned check now runs BEFORE the caller's logout
    /// instead of after it. A completed pass whose LOGOUT then hits a dead
    /// socket (which closes the session, i.e. `session_abandoned()`) must
    /// still read as a success — before the merge it was recorded as an
    /// "IMAP protocol error".
    #[test]
    fn a_logout_that_closes_the_session_does_not_fail_a_completed_pass() {
        struct ClosesOnLogout {
            closed: bool,
        }
        impl MailProvider for ClosesOnLogout {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![])
            }
            fn select(&mut self, _: &str) -> Result<MailboxState, ProviderError> {
                Ok(MailboxState {
                    uid_validity: 1,
                    uid_next: 1,
                    exists: 0,
                })
            }
            fn fetch_new(&mut self, _: Option<i64>) -> Result<Vec<RemoteMessage>, ProviderError> {
                Ok(vec![])
            }
            fn session_abandoned(&self) -> bool {
                self.closed
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                self.closed = true;
                Err(ProviderError::Network("connection reset".into()))
            }
        }

        let db = temp_db();
        sync_imap_json_with(&db, sync_cfg("acc-logout-closes"), |_cfg| {
            Ok(ClosesOnLogout { closed: false })
        })
        .expect("the pass completes");
        let state = db
            .get_account_sync_state("acc-logout-closes")
            .unwrap()
            .unwrap();
        assert!(
            state.last_success_at.is_some(),
            "a failed LOGOUT after a completed pass is not a sync failure"
        );
        assert_eq!(state.consecutive_failures, 0);
    }

    ///: a session that ended because the CONNECTION is gone
    /// (a server BYE, a dropped TCP connection, an I/O EOF) is a
    /// network-shaped failure — the account must read "network", not the
    /// "protocol" every abandoned session used to record regardless of
    /// cause.
    #[test]
    fn a_session_closed_by_the_server_is_a_network_account_failure() {
        let db = temp_db();
        let json = sync_imap_json_with(&db, sync_cfg("acc-closed"), |_cfg| {
            Ok(AbandonedWithReason {
                inner: MockProvider {
                    state: MailboxState {
                        uid_validity: 1,
                        uid_next: 1,
                        exists: 0,
                    },
                    messages: vec![],
                },
                reason: castellum_providers::SessionAbandonReason::Closed,
            })
        })
        .expect("per-mailbox failures keep the pass Ok");
        assert!(json.contains("\"mailboxes\""));

        let state = db.get_account_sync_state("acc-closed").unwrap().unwrap();
        assert!(state.last_success_at.is_none(), "not recorded as a success");
        assert_eq!(state.last_error_kind.as_deref(), Some("network"));
        assert_eq!(
            state.last_error_redacted.as_deref(),
            Some("network connection failed")
        );
    }

    /// Same tag, different cause: isolation gave up trying to recover a
    /// poisoned or closed session — its reconnect budget was already spent,
    /// or the reconnect it spent itself failed to reconnect. Either way the
    /// account could no longer get a working session back, which is
    /// "network", not the original decode error.
    #[test]
    fn a_session_abandoned_with_no_reconnect_left_is_a_network_account_failure() {
        let db = temp_db();
        let json = sync_imap_json_with(&db, sync_cfg("acc-budget"), |_cfg| {
            Ok(AbandonedWithReason {
                inner: MockProvider {
                    state: MailboxState {
                        uid_validity: 1,
                        uid_next: 1,
                        exists: 0,
                    },
                    messages: vec![],
                },
                reason: castellum_providers::SessionAbandonReason::BudgetExhausted,
            })
        })
        .expect("per-mailbox failures keep the pass Ok");
        assert!(json.contains("\"mailboxes\""));

        let state = db.get_account_sync_state("acc-budget").unwrap().unwrap();
        assert_eq!(state.last_error_kind.as_deref(), Some("network"));
    }

    /// Review nit 4 at the sync layer: even if a provider hands back a UID
    /// outside the requested span (RFC 3501's `n:*` quirk names the LAST
    /// UID when nothing is at or above n), a sparse incremental pass must
    /// not re-import that old message as new.
    #[test]
    fn a_sparse_pass_never_reimports_a_uid_outside_the_span() {
        let db = temp_db();
        let account_id = "acc-sparse-quirk";
        let mailbox_id = format!("{account_id}:INBOX");
        seed_synced_mailbox(&db, account_id, "INBOX", 1501);
        db.upsert_messages_batch(&[new_message_from_descriptor(
            account_id,
            &mailbox_id,
            remote_descriptor(400, "old"),
        )])
        .unwrap();

        let mut mock = SparseMock {
            out_of_span: vec![400],
            ..SparseMock::new(
                MailboxState {
                    uid_validity: 5,
                    uid_next: 3000,
                    exists: 1,
                },
                vec![remote_descriptor(400, "old")],
            )
        };
        let report = sync_with_provider(&db, account_id, "t@l.test", None, &mut mock, "h").unwrap();
        assert_eq!(mock.search_calls, vec![(1501, 2999)]);
        assert!(
            mock.list_calls.is_empty(),
            "nothing in the span: no fetch, no re-import ({:?})",
            mock.list_calls
        );
        assert_eq!(report.mailboxes[0].fetched, 0);
    }

    #[test]
    fn descriptor_failure_is_reported_without_blocking_later_mailboxes() {
        struct DescriptorFailureMock {
            selected: String,
        }

        impl MailProvider for DescriptorFailureMock {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![
                    rmb("INBOX", "INBOX", Some("inbox"), Some("/"), true, true),
                    rmb("Archive", "Archive", None, Some("/"), true, true),
                ])
            }
            fn select(&mut self, path: &str) -> Result<MailboxState, ProviderError> {
                self.selected = path.into();
                Ok(MailboxState {
                    uid_validity: 1,
                    uid_next: 2,
                    exists: 1,
                })
            }
            fn fetch_new(
                &mut self,
                _after_uid: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                unreachable!("descriptor-capable provider must not use legacy fetch")
            }
            fn fetch_new_descriptors(
                &mut self,
                _after_uid: Option<i64>,
            ) -> Result<Vec<RemoteMessageDescriptor>, ProviderError> {
                if self.selected == "INBOX" {
                    Err(ProviderError::Protocol("inbox descriptor failure".into()))
                } else {
                    Ok(vec![remote_descriptor(1, "Archive message")])
                }
            }
            fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
                Ok(vec![UidFlags {
                    keywords: None,
                    uid: 1,
                    is_seen: false,
                    is_flagged: false,
                }])
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }

        let db = temp_db();
        let mut provider = DescriptorFailureMock {
            selected: String::new(),
        };

        let report = sync_with_provider(
            &db,
            "acc",
            "test@local.test",
            None,
            &mut provider,
            "test-host",
        )
        .unwrap();

        assert_eq!(report.mailboxes.len(), 2);
        assert!(report.mailboxes[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("descriptor failure")));
        assert_eq!(report.mailboxes[1].fetched, 1);
        assert_eq!(report.mailboxes[1].error, None);
        assert!(db.get_message_detail("acc:Archive:1").unwrap().is_some());

        // Durable diagnostics: the failing INBOX records a redacted fetch-phase
        // failure; the healthy Archive still records a success.
        let states = db.list_mailbox_sync_states("acc").unwrap();
        let inbox = states.iter().find(|s| s.mailbox_id == "acc:INBOX").unwrap();
        assert_eq!(inbox.last_error_kind.as_deref(), Some("protocol"));
        assert_eq!(inbox.last_error_phase.as_deref(), Some("fetch"));
        assert_eq!(inbox.consecutive_failures, 1);
        assert!(inbox.last_success_at.is_none());
        let archive = states
            .iter()
            .find(|s| s.mailbox_id == "acc:Archive")
            .unwrap();
        assert!(archive.last_success_at.is_some());
        assert_eq!(archive.consecutive_failures, 0);
    }

    #[test]
    fn hydration_failure_checkpoints_descriptors_and_continues() {
        struct HydrationFailureMock {
            selected: String,
        }

        impl MailProvider for HydrationFailureMock {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![
                    rmb("INBOX", "INBOX", Some("inbox"), Some("/"), true, true),
                    rmb("Archive", "Archive", None, Some("/"), true, true),
                ])
            }
            fn select(&mut self, path: &str) -> Result<MailboxState, ProviderError> {
                self.selected = path.into();
                Ok(MailboxState {
                    uid_validity: 7,
                    uid_next: 2,
                    exists: 1,
                })
            }
            fn fetch_new(
                &mut self,
                _after_uid: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                unreachable!("descriptor-capable provider must not use legacy fetch")
            }
            fn fetch_new_descriptors(
                &mut self,
                after_uid: Option<i64>,
            ) -> Result<Vec<RemoteMessageDescriptor>, ProviderError> {
                Ok(if after_uid.is_none() {
                    vec![remote_descriptor(1, &format!("{} message", self.selected))]
                } else {
                    vec![]
                })
            }
            fn fetch_bodies(&mut self, uids: &[i64]) -> Result<Vec<RemoteMessage>, ProviderError> {
                if self.selected == "INBOX" {
                    Err(ProviderError::Protocol("inbox body failure".into()))
                } else {
                    Ok(uids.iter().map(|uid| msg(*uid)).collect())
                }
            }
            fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
                Ok(vec![UidFlags {
                    keywords: None,
                    uid: 1,
                    is_seen: false,
                    is_flagged: false,
                }])
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }

        let db = temp_db();
        let mut provider = HydrationFailureMock {
            selected: String::new(),
        };

        let report = sync_with_provider(
            &db,
            "acc",
            "test@local.test",
            None,
            &mut provider,
            "test-host",
        )
        .unwrap();

        assert_eq!(report.mailboxes.len(), 2);
        assert!(report.mailboxes[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("body failure")));
        assert_eq!(report.mailboxes[0].descriptors_fetched, 1);
        assert_eq!(report.mailboxes[1].bodies_fetched, 1);
        assert_eq!(report.mailboxes[1].error, None);
        assert_eq!(
            db.get_sync_cursor("acc", "acc:INBOX")
                .unwrap()
                .unwrap()
                .uid_next,
            Some(2)
        );
        assert!(
            !db.get_message_detail("acc:INBOX:1")
                .unwrap()
                .unwrap()
                .header
                .has_body
        );
        assert!(
            db.get_message_detail("acc:Archive:1")
                .unwrap()
                .unwrap()
                .header
                .has_body
        );

        // Durable diagnostics: the in-sync body-fetch failure records the
        // hydrate phase and skips the success site for that mailbox.
        let states = db.list_mailbox_sync_states("acc").unwrap();
        let inbox = states.iter().find(|s| s.mailbox_id == "acc:INBOX").unwrap();
        assert_eq!(inbox.last_error_kind.as_deref(), Some("protocol"));
        assert_eq!(inbox.last_error_phase.as_deref(), Some("hydrate"));
        assert!(inbox.last_success_at.is_none());
        let archive = states
            .iter()
            .find(|s| s.mailbox_id == "acc:Archive")
            .unwrap();
        assert!(archive.last_success_at.is_some());
    }

    #[test]
    fn hydrated_full_body_persists_attachment_descriptors() {
        let db = temp_db();
        seed_account_mailbox(&db);
        let raw = b"From: alice@example.com\r\n\
To: test@local.test\r\n\
Subject: Attachment metadata\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=BOUNDARY\r\n\
\r\n\
--BOUNDARY\r\n\
Content-Type: text/plain\r\n\
\r\n\
See attached.\r\n\
--BOUNDARY\r\n\
Content-Type: application/pdf; name=brief.pdf\r\n\
Content-Disposition: attachment; filename=brief.pdf\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
UERG\r\n\
--BOUNDARY--\r\n";

        let (message, ..) = new_message_from_raw(
            "acc",
            "acc:INBOX",
            RemoteMessage {
                uid: 9,
                raw: raw.to_vec(),
                is_seen: false,
                is_flagged: false,
            },
            "test-host",
        );
        db.upsert_message(&message).unwrap();

        let attachments = db.list_message_attachments("acc:INBOX:9").unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].filename.as_deref(), Some("brief.pdf"));
        assert_eq!(
            attachments[0].content_type.as_deref(),
            Some("application/pdf")
        );
        assert_eq!(attachments[0].size_bytes, Some(3));
    }

    /// MM-1c: hydrating a body stores the policy-filtered headers plus a
    /// capture row, and a second hydration REPLACES (never merges or duplicates).
    #[test]
    fn hydrated_body_stores_headers_and_rehydration_replaces_them() {
        struct HeaderMock {
            raw: Vec<u8>,
        }
        impl MailProvider for HeaderMock {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![])
            }
            fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
                Ok(MailboxState {
                    uid_validity: 1,
                    uid_next: 2,
                    exists: 1,
                })
            }
            fn fetch_new(
                &mut self,
                _after_uid: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                Err(ProviderError::Protocol("unexpected".into()))
            }
            fn fetch_bodies(&mut self, uids: &[i64]) -> Result<Vec<RemoteMessage>, ProviderError> {
                Ok(uids
                    .iter()
                    .map(|uid| RemoteMessage {
                        uid: *uid,
                        raw: self.raw.clone(),
                        is_seen: false,
                        is_flagged: false,
                    })
                    .collect())
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }
        let first = b"From: a@example.com\r\nTo: me@local.test\r\nSubject: Hi\r\n\
Message-ID: <1@x>\r\nReceived: from a by b\r\nAuthorization: Bearer topsecret\r\n\
List-Id: Dev <dev.example.org>\r\nX-Mailer: Foo\r\n\r\nBody.\r\n";
        let second = b"From: a@example.com\r\nTo: me@local.test\r\nSubject: Hi\r\n\
List-Id: Dev <dev.example.org>\r\n\r\nBody.\r\n";

        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor(&db, 1, 2);
        db.upsert_message(&descriptor_message(1)).unwrap();
        assert_eq!(db.message_header_coverage().unwrap().captured, 0);

        let mut provider = HeaderMock {
            raw: first.to_vec(),
        };
        hydrate_message_body_with_provider(&db, "acc", "acc:INBOX:1", &mut provider, "test-host")
            .unwrap();
        let names = |db: &Database| -> Vec<String> {
            db.list_stored_header_rows("acc:INBOX:1")
                .unwrap()
                .into_iter()
                .map(|(n, _, v)| format!("{n}={v}"))
                .collect()
        };
        assert_eq!(
            names(&db),
            vec!["list-id=Dev <dev.example.org>", "x-mailer=Foo"],
            "denylisted and credential headers are never stored"
        );
        let c = db.message_header_coverage().unwrap();
        assert_eq!((c.captured, c.total), (1, 1));

        // Re-hydration with fewer headers replaces the stored set.
        let mut provider = HeaderMock {
            raw: second.to_vec(),
        };
        let (nm, extras) = new_message_from_raw(
            "acc",
            "acc:INBOX",
            provider.fetch_bodies(&[1]).unwrap().remove(0),
            "test-host",
        );
        db.upsert_message(&nm).unwrap();
        persist_message_headers(&db, &nm.id, &extras);
        assert_eq!(names(&db), vec!["list-id=Dev <dev.example.org>"]);
        assert_eq!(db.message_header_coverage().unwrap().captured, 1);
    }

    #[test]
    fn hydrate_descriptor_only_message_body_on_open() {
        struct BodyOnOpenMock {
            selected_paths: Vec<String>,
            fetched_body_uids: Vec<i64>,
        }

        impl MailProvider for BodyOnOpenMock {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![])
            }
            fn select(&mut self, path: &str) -> Result<MailboxState, ProviderError> {
                self.selected_paths.push(path.to_string());
                Ok(MailboxState {
                    uid_validity: 1,
                    uid_next: 2,
                    exists: 1,
                })
            }
            fn fetch_new(
                &mut self,
                _after_uid: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                Err(ProviderError::Protocol(
                    "new-message fetch should not run".into(),
                ))
            }
            fn fetch_bodies(&mut self, uids: &[i64]) -> Result<Vec<RemoteMessage>, ProviderError> {
                self.fetched_body_uids = uids.to_vec();
                Ok(uids.iter().map(|uid| msg(*uid)).collect())
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }

        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor(&db, 1, 2);
        db.upsert_message(&descriptor_message(1)).unwrap();
        assert!(
            !db.get_message_detail("acc:INBOX:1")
                .unwrap()
                .unwrap()
                .header
                .has_body
        );

        let mut provider = BodyOnOpenMock {
            selected_paths: vec![],
            fetched_body_uids: vec![],
        };
        let detail = hydrate_message_body_with_provider(
            &db,
            "acc",
            "acc:INBOX:1",
            &mut provider,
            "test-host",
        )
        .unwrap()
        .unwrap();

        assert_eq!(provider.selected_paths, vec!["INBOX"]);
        assert_eq!(provider.fetched_body_uids, vec![1]);
        assert!(detail.header.has_body);
        assert_eq!(detail.header.subject.as_deref(), Some("Message 1"));
        assert_eq!(detail.text_body.as_deref(), Some("Body of message 1.\r\n"));
    }

    #[test]
    fn hydrate_rejects_uid_reuse_after_uidvalidity_change() {
        struct ResetMock {
            fetch_called: bool,
        }

        impl MailProvider for ResetMock {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![])
            }
            fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
                Ok(MailboxState {
                    uid_validity: 2,
                    uid_next: 2,
                    exists: 1,
                })
            }
            fn fetch_new(
                &mut self,
                _after_uid: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                unreachable!("new-message fetch must not run")
            }
            fn fetch_bodies(&mut self, _uids: &[i64]) -> Result<Vec<RemoteMessage>, ProviderError> {
                self.fetch_called = true;
                Ok(vec![msg(1)])
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }

        let db = temp_db();
        seed_account_mailbox(&db);
        seed_inbox_cursor(&db, 1, 2);
        db.upsert_message(&descriptor_message(1)).unwrap();
        let mut provider = ResetMock {
            fetch_called: false,
        };

        let error = hydrate_message_body_with_provider(
            &db,
            "acc",
            "acc:INBOX:1",
            &mut provider,
            "test-host",
        )
        .unwrap_err();
        assert!(error.to_string().contains("UIDVALIDITY"), "{error}");
        assert!(!provider.fetch_called);
        assert!(
            !db.get_message_detail("acc:INBOX:1")
                .unwrap()
                .unwrap()
                .header
                .has_body
        );
    }

    #[test]
    fn hydrate_message_body_skips_already_hydrated_rows() {
        struct NoFetchMock {
            select_called: bool,
            fetch_called: bool,
        }

        impl MailProvider for NoFetchMock {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![])
            }
            fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
                self.select_called = true;
                Err(ProviderError::Protocol(
                    "select should not run for hydrated rows".into(),
                ))
            }
            fn fetch_new(
                &mut self,
                _after_uid: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                Err(ProviderError::Protocol(
                    "new-message fetch should not run".into(),
                ))
            }
            fn fetch_bodies(&mut self, _uids: &[i64]) -> Result<Vec<RemoteMessage>, ProviderError> {
                self.fetch_called = true;
                Err(ProviderError::Protocol(
                    "body fetch should not run for hydrated rows".into(),
                ))
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }

        let db = temp_db();
        seed_account_mailbox(&db);
        let (nm1, ..) = new_message_from_raw("acc", "acc:INBOX", msg(1), "test-host");
        db.upsert_message(&nm1).unwrap();

        let mut provider = NoFetchMock {
            select_called: false,
            fetch_called: false,
        };
        let detail = hydrate_message_body_with_provider(
            &db,
            "acc",
            "acc:INBOX:1",
            &mut provider,
            "test-host",
        )
        .unwrap()
        .unwrap();

        assert!(detail.header.has_body);
        assert!(!provider.select_called);
        assert!(!provider.fetch_called);
    }

    #[test]
    fn hydrate_message_body_returns_none_for_missing_message() {
        struct NoFetchMock;

        impl MailProvider for NoFetchMock {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![])
            }
            fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
                Err(ProviderError::Protocol(
                    "select should not run for a missing message".into(),
                ))
            }
            fn fetch_new(
                &mut self,
                _after_uid: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                Err(ProviderError::Protocol(
                    "new-message fetch should not run".into(),
                ))
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }

        let db = temp_db();
        let mut provider = NoFetchMock;
        assert!(hydrate_message_body_with_provider(
            &db,
            "acc",
            "missing",
            &mut provider,
            "test-host"
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn hydrate_message_body_json_returns_null_without_network_for_missing_message() {
        let db = temp_db();
        let config = r#"{
            "account_id":"acc",
            "email":"test@local.test",
            "host":"192.0.2.1",
            "port":1,
            "username":"u",
            "password":"p",
            "use_tls":false
        }"#;

        assert_eq!(
            hydrate_message_body_json(&db, config, "missing").unwrap(),
            "null"
        );
    }

    #[test]
    fn hydrate_message_body_json_skips_network_for_already_hydrated_message() {
        let db = temp_db();
        seed_account_mailbox(&db);
        let (nm1, ..) = new_message_from_raw("acc", "acc:INBOX", msg(1), "test-host");
        db.upsert_message(&nm1).unwrap();
        let config = r#"{
            "account_id":"acc",
            "email":"test@local.test",
            "host":"192.0.2.1",
            "port":1,
            "username":"u",
            "password":"p",
            "use_tls":false
        }"#;

        let json = hydrate_message_body_json(&db, config, "acc:INBOX:1").unwrap();
        assert!(json.contains("\"has_body\":true"));
        assert!(json.contains("\"text_body\":\"Body of message 1."));
    }

    #[test]
    fn descriptor_sync_hydrates_only_a_bounded_newest_body_batch() {
        struct HydrationMock {
            state: MailboxState,
            descriptors: Vec<RemoteMessageDescriptor>,
            fetched_body_uids: Vec<i64>,
        }

        impl MailProvider for HydrationMock {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![rmb(
                    "INBOX",
                    "INBOX",
                    Some("inbox"),
                    Some("/"),
                    true,
                    true,
                )])
            }
            fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
                Ok(self.state)
            }
            fn fetch_new(
                &mut self,
                _after_uid: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                Err(ProviderError::Protocol(
                    "legacy body fetch should not run".into(),
                ))
            }
            fn fetch_new_descriptors(
                &mut self,
                after_uid: Option<i64>,
            ) -> Result<Vec<RemoteMessageDescriptor>, ProviderError> {
                Ok(self
                    .descriptors
                    .iter()
                    .filter(|d| after_uid.is_none_or(|uid| d.uid > uid))
                    .cloned()
                    .collect())
            }
            fn fetch_bodies(&mut self, uids: &[i64]) -> Result<Vec<RemoteMessage>, ProviderError> {
                self.fetched_body_uids = uids.to_vec();
                Ok(uids.iter().map(|uid| msg(*uid)).collect())
            }
            fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
                Ok(self
                    .descriptors
                    .iter()
                    .map(|d| UidFlags {
                        keywords: None,
                        uid: d.uid,
                        is_seen: d.is_seen,
                        is_flagged: d.is_flagged,
                    })
                    .collect())
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }

        fn descriptor(uid: i64) -> RemoteMessageDescriptor {
            RemoteMessageDescriptor {
                keywords: None,
                uid,
                size_bytes: Some(10_000 + uid),
                has_attachments: Some(false),
                is_seen: false,
                is_flagged: false,
                rfc822_message_id: Some(format!("<d{uid}@example.com>")),
                subject: Some(format!("Descriptor {uid}")),
                from: Some(RemoteAddress {
                    name: Some("Alice".into()),
                    address: "alice@example.com".into(),
                }),
                to: vec![],
                cc: vec![],
                bcc: vec![],
                sent_at: Some(1_788_000_000 + uid),
                references: vec![],
            }
        }

        let db = temp_db();
        let descriptors = (1..=30).map(descriptor).collect::<Vec<_>>();
        let mut mock = HydrationMock {
            state: MailboxState {
                uid_validity: 1,
                uid_next: 31,
                exists: 30,
            },
            descriptors,
            fetched_body_uids: vec![],
        };

        let report =
            sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
                .unwrap();

        let expected_uids = (6..=30).rev().collect::<Vec<_>>();
        assert_eq!(mock.fetched_body_uids, expected_uids);
        assert_eq!(report.mailboxes[0].fetched, 30);
        assert_eq!(report.mailboxes[0].descriptors_fetched, 30);
        assert_eq!(report.mailboxes[0].bodies_fetched, 25);
        assert_eq!(
            report.mailboxes[0].bytes_fetched,
            (6..=30).map(|uid| raw_email(uid).len() as i64).sum::<i64>()
        );

        let hydrated = db.get_message_detail("acc:INBOX:30").unwrap().unwrap();
        assert_eq!(hydrated.header.subject.as_deref(), Some("Message 30"));
        assert!(hydrated.text_body.is_some());

        let descriptor_only = db.get_message_detail("acc:INBOX:5").unwrap().unwrap();
        assert_eq!(
            descriptor_only.header.subject.as_deref(),
            Some("Descriptor 5")
        );
        assert!(descriptor_only.text_body.is_none());
    }

    /// Shared fixtures for the bounded background hydration-runner tests.
    mod hydration_runner {
        use super::*;
        use std::collections::HashMap;
        use std::path::PathBuf;

        pub(super) fn temp_db_path() -> PathBuf {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = castellum_testutil::scratch_root().join(format!(
                "castellum-hydration-test-{}-{}.db",
                std::process::id(),
                n
            ));
            let _ = std::fs::remove_file(&path);
            path
        }

        /// Seed one mailbox with descriptor-only rows (no body) and a matching
        /// sync cursor, exactly as descriptor-first sync leaves them.
        pub(super) fn seed_mailbox(
            db: &Database,
            path: &str,
            role: Option<&str>,
            uid_validity: i64,
            uids: std::ops::RangeInclusive<i64>,
        ) {
            let sized: Vec<(i64, Option<i64>)> = uids.map(|u| (u, Some(1000 + u))).collect();
            seed_sized_mailbox(db, path, role, uid_validity, &sized);
        }

        /// Like [`seed_mailbox`], but with explicit per-UID stored sizes
        /// (`None` = RFC822.SIZE unknown) for byte-budget tests.
        pub(super) fn seed_sized_mailbox(
            db: &Database,
            path: &str,
            role: Option<&str>,
            uid_validity: i64,
            uids: &[(i64, Option<i64>)],
        ) {
            let mailbox_id = format!("acc:{path}");
            db.upsert_account(&Account {
                id: "acc".into(),
                email: "test@local.test".into(),
                display_name: None,
                provider: "imap".into(),
            })
            .unwrap();
            // Reconciliation tests queue local flag/mark-all intent; needs writes.
            db.set_account_write_enabled("acc", true).unwrap();
            db.upsert_mailbox(&Mailbox {
                id: mailbox_id.clone(),
                account_id: "acc".into(),
                path: path.into(),
                name: path.into(),
                role: role.map(str::to_string),
                delimiter: Some("/".into()),
                subscribed: true,
                selectable: true,
                sync_by_default: true,
            })
            .unwrap();
            let uid_next = uids.iter().map(|(u, _)| *u).max().unwrap_or(0) + 1;
            for &(uid, size_bytes) in uids {
                let descriptor = RemoteMessageDescriptor {
                    keywords: None,
                    uid,
                    size_bytes,
                    has_attachments: Some(false),
                    is_seen: false,
                    is_flagged: false,
                    rfc822_message_id: Some(format!("d{uid}@example.com")),
                    subject: Some(format!("Descriptor {uid}")),
                    from: Some(RemoteAddress {
                        name: Some("Alice".into()),
                        address: "alice@example.com".into(),
                    }),
                    to: vec![],
                    cc: vec![],
                    bcc: vec![],
                    sent_at: Some(1_788_000_000 + uid),
                    references: vec![],
                };
                let nm = new_message_from_descriptor("acc", &mailbox_id, descriptor);
                db.upsert_message(&nm).unwrap();
            }
            db.set_sync_cursor(
                "acc",
                &mailbox_id,
                &SyncCursor {
                    uid_validity: Some(uid_validity),
                    uid_next: Some(uid_next),
                    highest_modseq: None,
                    descriptor_checkpoint_uid: None,
                },
            )
            .unwrap();
        }

        /// Scriptable multi-mailbox provider for runner tests. Records every
        /// select and body-fetch so tests can assert bounded, non-repeating
        /// work; per-path failures and UIDVALIDITY overrides model provider
        /// faults and identity changes.
        #[derive(Default)]
        pub(super) struct RunnerMock {
            /// UIDVALIDITY reported by `select`, per path (default 1).
            pub uid_validity: HashMap<String, i64>,
            /// Paths whose body fetch fails with a protocol error.
            pub fail_fetch: std::collections::HashSet<String>,
            /// When set, `fetch_bodies` rewrites this mailbox's stored cursor
            /// through a second DB handle before returning — models a
            /// concurrent sync detecting a UIDVALIDITY reset mid-fetch.
            pub reset_cursor_during_fetch: Option<(PathBuf, String)>,
            pub selected: Option<String>,
            pub select_calls: Vec<String>,
            /// (path, uids) for every fetch_bodies call.
            pub fetch_calls: Vec<(String, Vec<i64>)>,
        }

        impl MailProvider for RunnerMock {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Err(ProviderError::Protocol(
                    "hydration runner must not list mailboxes".into(),
                ))
            }
            fn select(&mut self, path: &str) -> Result<MailboxState, ProviderError> {
                self.select_calls.push(path.to_string());
                self.selected = Some(path.to_string());
                Ok(MailboxState {
                    uid_validity: *self.uid_validity.get(path).unwrap_or(&1),
                    uid_next: 1000,
                    exists: 1,
                })
            }
            fn fetch_new(
                &mut self,
                _after_uid: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                Err(ProviderError::Protocol(
                    "hydration runner must not fetch new messages".into(),
                ))
            }
            fn fetch_bodies(&mut self, uids: &[i64]) -> Result<Vec<RemoteMessage>, ProviderError> {
                let path = self.selected.clone().unwrap_or_default();
                self.fetch_calls.push((path.clone(), uids.to_vec()));
                if self.fail_fetch.contains(&path) {
                    return Err(ProviderError::Protocol("body fetch failed".into()));
                }
                if let Some((db_path, mailbox_id)) = &self.reset_cursor_during_fetch {
                    let db = Database::open(db_path).unwrap();
                    db.set_sync_cursor(
                        "acc",
                        mailbox_id,
                        &SyncCursor {
                            uid_validity: Some(999),
                            uid_next: Some(1),
                            highest_modseq: None,
                            descriptor_checkpoint_uid: None,
                        },
                    )
                    .unwrap();
                }
                Ok(uids.iter().map(|uid| msg(*uid)).collect())
            }
            fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
                Err(ProviderError::Protocol(
                    "hydration runner must not reconcile flags".into(),
                ))
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }
    }

    #[test]
    fn hydration_pass_hydrates_one_bounded_newest_batch() {
        let db = temp_db();
        hydration_runner::seed_mailbox(&db, "INBOX", Some("inbox"), 1, 1..=60);
        let mut mock = hydration_runner::RunnerMock::default();

        let report = hydrate_pending_bodies_with_provider(
            &db,
            "acc",
            &mut mock,
            HYDRATION_PASS_DEFAULT_BYTE_BUDGET,
            "test-host",
        )
        .unwrap();

        assert_eq!(report.account_id, "acc");
        assert_eq!(report.requested, 25);
        assert_eq!(report.hydrated, 25);
        assert_eq!(report.remaining, 35);
        assert_eq!(report.mailboxes.len(), 1);
        let mb = &report.mailboxes[0];
        assert_eq!(mb.path, "INBOX");
        assert_eq!(mb.requested, 25);
        assert_eq!(mb.hydrated, 25);
        assert_eq!(mb.remaining, 35);
        assert!(mb.error.is_none());
        let expected_bytes: i64 = (36..=60).map(|uid| raw_email(uid).len() as i64).sum();
        assert_eq!(report.bytes_fetched, expected_bytes);
        // Newest-first: UIDs 60..36 hydrate before older descriptor rows.
        assert_eq!(mock.fetch_calls.len(), 1);
        assert_eq!(mock.fetch_calls[0].1, (36..=60).rev().collect::<Vec<_>>());
        assert!(
            db.get_message_detail("acc:INBOX:60")
                .unwrap()
                .unwrap()
                .header
                .has_body
        );
        assert!(
            !db.get_message_detail("acc:INBOX:35")
                .unwrap()
                .unwrap()
                .header
                .has_body
        );
    }

    #[test]
    fn repeated_hydration_passes_drain_backlog_without_refetch() {
        let db = temp_db();
        hydration_runner::seed_mailbox(&db, "INBOX", Some("inbox"), 1, 1..=60);
        let mut mock = hydration_runner::RunnerMock::default();

        let mut all_fetched: Vec<i64> = Vec::new();
        let mut passes = 0;
        loop {
            let report = hydrate_pending_bodies_with_provider(
                &db,
                "acc",
                &mut mock,
                HYDRATION_PASS_DEFAULT_BYTE_BUDGET,
                "test-host",
            )
            .unwrap();
            passes += 1;
            if report.remaining == 0 {
                break;
            }
            assert!(passes < 10, "backlog must drain in bounded passes");
        }
        for (_, uids) in &mock.fetch_calls {
            all_fetched.extend(uids);
        }

        assert_eq!(passes, 3, "60 missing bodies = 25 + 25 + 10");
        assert_eq!(all_fetched.len(), 60, "every UID fetched exactly once");
        let mut sorted = all_fetched.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 60, "no UID re-fetched across passes");
        assert!(db.list_hydration_backlog("acc").unwrap().is_empty());
    }

    #[test]
    fn hydration_restart_resumes_from_durable_queue() {
        let path = hydration_runner::temp_db_path();
        {
            let db = Database::open(&path).unwrap();
            hydration_runner::seed_mailbox(&db, "INBOX", Some("inbox"), 1, 1..=40);
            let mut mock = hydration_runner::RunnerMock::default();
            let report = hydrate_pending_bodies_with_provider(
                &db,
                "acc",
                &mut mock,
                HYDRATION_PASS_DEFAULT_BYTE_BUDGET,
                "test-host",
            )
            .unwrap();
            assert_eq!(report.hydrated, 25);
            assert_eq!(report.remaining, 15);
        }

        // "Restart": a brand-new process would reopen the same profile. The
        // second pass must request only the still-missing older UIDs.
        let db = Database::open(&path).unwrap();
        let mut mock = hydration_runner::RunnerMock::default();
        let report = hydrate_pending_bodies_with_provider(
            &db,
            "acc",
            &mut mock,
            HYDRATION_PASS_DEFAULT_BYTE_BUDGET,
            "test-host",
        )
        .unwrap();
        assert_eq!(report.requested, 15);
        assert_eq!(report.hydrated, 15);
        assert_eq!(report.remaining, 0);
        assert_eq!(mock.fetch_calls.len(), 1);
        assert_eq!(mock.fetch_calls[0].1, (1..=15).rev().collect::<Vec<_>>());
    }

    #[test]
    fn hydration_skips_already_hydrated_and_marker_rows() {
        let db = temp_db();
        hydration_runner::seed_mailbox(&db, "INBOX", Some("inbox"), 1, 1..=10);
        // UID 10 hydrates with a real body; UID 9 is an authoritative
        // empty/attachment-only parse, which R4i marks hydrated via a body row.
        let (nm10, ..) = new_message_from_raw("acc", "acc:INBOX", msg(10), "test-host");
        db.upsert_message(&nm10).unwrap();
        let mut marker = new_message_from_descriptor(
            "acc",
            "acc:INBOX",
            RemoteMessageDescriptor {
                keywords: None,
                uid: 9,
                size_bytes: Some(1009),
                has_attachments: Some(false),
                is_seen: false,
                is_flagged: false,
                rfc822_message_id: Some("d9@example.com".into()),
                subject: Some("Descriptor 9".into()),
                from: None,
                to: vec![],
                cc: vec![],
                bcc: vec![],
                sent_at: Some(1_788_000_009),
                references: vec![],
            },
        );
        marker.attachments = Some(vec![]);
        db.upsert_message(&marker).unwrap();

        let mut mock = hydration_runner::RunnerMock::default();
        let report = hydrate_pending_bodies_with_provider(
            &db,
            "acc",
            &mut mock,
            HYDRATION_PASS_DEFAULT_BYTE_BUDGET,
            "test-host",
        )
        .unwrap();

        assert_eq!(report.requested, 8);
        assert_eq!(report.remaining, 0);
        let fetched = &mock.fetch_calls[0].1;
        assert!(!fetched.contains(&10), "hydrated row must not re-fetch");
        assert!(!fetched.contains(&9), "empty-body marker must not re-fetch");
    }

    #[test]
    fn hydration_pass_total_cap_bounds_work_across_mailboxes() {
        let db = temp_db();
        // Six mailboxes with 25 missing each; inbox must be served first and
        // the pass must stop at the 100-body total budget.
        hydration_runner::seed_mailbox(&db, "Zeta", None, 1, 1..=25);
        hydration_runner::seed_mailbox(&db, "Alpha", None, 1, 1..=25);
        hydration_runner::seed_mailbox(&db, "Sent", Some("sent"), 1, 1..=25);
        hydration_runner::seed_mailbox(&db, "Trash", Some("trash"), 1, 1..=25);
        hydration_runner::seed_mailbox(&db, "Junk", Some("junk"), 1, 1..=25);
        hydration_runner::seed_mailbox(&db, "INBOX", Some("inbox"), 1, 1..=25);
        let mut mock = hydration_runner::RunnerMock::default();

        let report = hydrate_pending_bodies_with_provider(
            &db,
            "acc",
            &mut mock,
            HYDRATION_PASS_DEFAULT_BYTE_BUDGET,
            "test-host",
        )
        .unwrap();

        assert_eq!(report.requested, 100);
        assert_eq!(report.hydrated, 100);
        assert_eq!(report.remaining, 50);
        // Priority: inbox, then sent, then unroled (recent-activity ties break
        // by path), and junk/trash never enter a capped pass.
        let visited: Vec<&str> = mock.select_calls.iter().map(String::as_str).collect();
        assert_eq!(visited, vec!["INBOX", "Sent", "Alpha", "Zeta"]);
        assert!(!visited.contains(&"Junk"));
        assert!(!visited.contains(&"Trash"));
    }

    #[test]
    fn hydration_byte_budget_skips_oversized_and_still_hydrates_smaller_bodies() {
        let db = temp_db();
        // Newest first: uid 3 is far over the budget, uid 2 is small, uid 1
        // has no stored size (budgeted with the conservative estimate).
        hydration_runner::seed_sized_mailbox(
            &db,
            "INBOX",
            Some("inbox"),
            1,
            &[(1, None), (2, Some(2_048)), (3, Some(10 << 20))],
        );
        let mut mock = hydration_runner::RunnerMock::default();

        let report =
            hydrate_pending_bodies_with_provider(&db, "acc", &mut mock, 1 << 20, "test-host")
                .unwrap();

        // The oversized newest body is never requested and never starves the
        // smaller bodies behind it; it stays queued for body-on-open.
        assert_eq!(mock.fetch_calls.len(), 1);
        assert_eq!(mock.fetch_calls[0].1, vec![2, 1]);
        assert_eq!(report.requested, 2);
        assert_eq!(report.hydrated, 2);
        assert_eq!(report.remaining, 1);
        assert!(
            !db.get_message_detail("acc:INBOX:3")
                .unwrap()
                .unwrap()
                .header
                .has_body,
            "the oversized body must remain for explicit body-on-open hydration"
        );

        // A follow-up pass finds only the oversized candidate, requests
        // nothing, and reports zero progress — the caller's stop rule, not a
        // hot loop, ends the drain.
        let report =
            hydrate_pending_bodies_with_provider(&db, "acc", &mut mock, 1 << 20, "test-host")
                .unwrap();
        assert_eq!(report.requested, 0);
        assert_eq!(report.hydrated, 0);
        assert_eq!(report.remaining, 1);
        assert_eq!(mock.fetch_calls.len(), 1, "no further provider fetch");
    }

    #[test]
    fn hydration_byte_budget_bounds_first_pass_selection_across_mailboxes() {
        let db = temp_db();
        // INBOX: ten 300 KiB bodies; Alpha: ten more. Budget: 1 MiB for the
        // whole pass. The pass must select pre-fetch — three newest INBOX
        // bodies fit (900 KiB), the fourth would overflow — and Alpha gets
        // nothing because the remaining budget is under one body.
        let sizes: Vec<(i64, Option<i64>)> = (1..=10).map(|u| (u, Some(300 << 10))).collect();
        hydration_runner::seed_sized_mailbox(&db, "INBOX", Some("inbox"), 1, &sizes);
        hydration_runner::seed_sized_mailbox(&db, "Alpha", None, 1, &sizes);
        let mut mock = hydration_runner::RunnerMock::default();

        let report =
            hydrate_pending_bodies_with_provider(&db, "acc", &mut mock, 1 << 20, "test-host")
                .unwrap();

        assert_eq!(mock.fetch_calls.len(), 1, "only INBOX fetches");
        assert_eq!(mock.fetch_calls[0].0, "INBOX");
        assert_eq!(mock.fetch_calls[0].1, vec![10, 9, 8]);
        assert_eq!(
            mock.select_calls,
            vec!["INBOX"],
            "a mailbox with no budget-fitting candidate costs no SELECT"
        );
        assert_eq!(report.requested, 3);
        assert_eq!(report.hydrated, 3);
        assert_eq!(report.remaining, 17);
    }

    #[test]
    fn hydration_byte_budget_deserializes_from_config_json() {
        let with: ImapSyncConfig = serde_json::from_str(
            r#"{"account_id":"a","email":"e@x.test","host":"h","port":993,
                "username":"u","password":"p","hydration_byte_budget":12345}"#,
        )
        .unwrap();
        assert_eq!(with.hydration_byte_budget, Some(12345));
        let without: ImapSyncConfig = serde_json::from_str(
            r#"{"account_id":"a","email":"e@x.test","host":"h","port":993,
                "username":"u","password":"p"}"#,
        )
        .unwrap();
        assert_eq!(without.hydration_byte_budget, None);
    }

    #[test]
    fn hydration_mailbox_failure_is_isolated() {
        let db = temp_db();
        // INBOX fails; four full-batch mailboxes follow. All four must hydrate
        // completely, proving a failed mailbox neither stops later mailboxes
        // nor consumes any of the 100-body pass budget.
        hydration_runner::seed_mailbox(&db, "INBOX", Some("inbox"), 1, 1..=5);
        for path in ["Alpha", "Beta", "Gamma", "Delta"] {
            hydration_runner::seed_mailbox(&db, path, None, 1, 1..=25);
        }
        let mut mock = hydration_runner::RunnerMock::default();
        mock.fail_fetch.insert("INBOX".into());

        let report = hydrate_pending_bodies_with_provider(
            &db,
            "acc",
            &mut mock,
            HYDRATION_PASS_DEFAULT_BYTE_BUDGET,
            "test-host",
        )
        .unwrap();

        let inbox = report.mailboxes.iter().find(|m| m.path == "INBOX").unwrap();
        assert!(inbox
            .error
            .as_deref()
            .unwrap()
            .contains("body fetch failed"));
        assert_eq!(inbox.hydrated, 0);
        assert_eq!(inbox.remaining, 5);
        for path in ["Alpha", "Beta", "Gamma", "Delta"] {
            let mb = report.mailboxes.iter().find(|m| m.path == path).unwrap();
            assert!(mb.error.is_none());
            assert_eq!(mb.hydrated, 25, "{path} must get a full batch");
        }
        assert_eq!(report.hydrated, 100);
        assert_eq!(report.remaining, 5);
    }

    #[test]
    fn hydration_uidvalidity_mismatch_skips_without_fetch() {
        let db = temp_db();
        hydration_runner::seed_mailbox(&db, "INBOX", Some("inbox"), 1, 1..=5);
        hydration_runner::seed_mailbox(&db, "Archive", Some("archive"), 1, 1..=5);
        let mut mock = hydration_runner::RunnerMock::default();
        mock.uid_validity.insert("INBOX".into(), 2); // server reset identity

        let report = hydrate_pending_bodies_with_provider(
            &db,
            "acc",
            &mut mock,
            HYDRATION_PASS_DEFAULT_BYTE_BUDGET,
            "test-host",
        )
        .unwrap();

        let inbox = report.mailboxes.iter().find(|m| m.path == "INBOX").unwrap();
        assert!(inbox.error.as_deref().unwrap().contains("UIDVALIDITY"));
        assert!(
            mock.fetch_calls.iter().all(|(path, _)| path != "INBOX"),
            "no body fetch may run for a mailbox with changed identity"
        );
        let archive = report
            .mailboxes
            .iter()
            .find(|m| m.path == "Archive")
            .unwrap();
        assert_eq!(archive.hydrated, 5);
    }

    #[test]
    fn hydration_empty_backlog_short_circuits_without_network() {
        let db = temp_db();
        hydration_runner::seed_mailbox(&db, "INBOX", Some("inbox"), 1, 1..=2);
        let mut mock = hydration_runner::RunnerMock::default();
        let report = hydrate_pending_bodies_with_provider(
            &db,
            "acc",
            &mut mock,
            HYDRATION_PASS_DEFAULT_BYTE_BUDGET,
            "test-host",
        )
        .unwrap();
        assert_eq!(report.remaining, 0);

        // Fully hydrated: the inner pass makes zero provider calls…
        let mut idle_mock = hydration_runner::RunnerMock::default();
        let report = hydrate_pending_bodies_with_provider(
            &db,
            "acc",
            &mut idle_mock,
            HYDRATION_PASS_DEFAULT_BYTE_BUDGET,
            "test-host",
        )
        .unwrap();
        assert_eq!(
            report,
            HydrationReport {
                account_id: "acc".into(),
                mailboxes: vec![],
                requested: 0,
                hydrated: 0,
                bytes_fetched: 0,
                remaining: 0,
                elapsed_ms: report.elapsed_ms,
            }
        );
        assert!(idle_mock.select_calls.is_empty());
        assert!(idle_mock.fetch_calls.is_empty());

        // …and the JSON wrapper never even connects (unroutable host succeeds).
        let config = r#"{
            "account_id":"acc",
            "email":"test@local.test",
            "host":"192.0.2.1",
            "port":1,
            "username":"u",
            "password":"p",
            "use_tls":false
        }"#;
        let json = hydrate_pending_bodies_json(&db, config).unwrap();
        assert!(json.contains("\"remaining\":0"));
    }

    #[test]
    fn hydration_stale_batch_dropped_on_uidvalidity_change_mid_fetch() {
        let path = hydration_runner::temp_db_path();
        let db = Database::open(&path).unwrap();
        hydration_runner::seed_mailbox(&db, "INBOX", Some("inbox"), 1, 1..=5);
        let mut mock = hydration_runner::RunnerMock {
            reset_cursor_during_fetch: Some((path.clone(), "acc:INBOX".into())),
            ..Default::default()
        };

        let report = hydrate_pending_bodies_with_provider(
            &db,
            "acc",
            &mut mock,
            HYDRATION_PASS_DEFAULT_BYTE_BUDGET,
            "test-host",
        )
        .unwrap();

        let inbox = &report.mailboxes[0];
        assert!(inbox
            .error
            .as_deref()
            .unwrap()
            .contains("UIDVALIDITY changed during body fetch"));
        assert_eq!(report.hydrated, 0);
        // Fetched bodies from the old UID space must not attach to local rows.
        for uid in 1..=5 {
            assert!(
                !db.get_message_detail(&format!("acc:INBOX:{uid}"))
                    .unwrap()
                    .unwrap()
                    .header
                    .has_body
            );
        }
    }

    #[test]
    fn uidvalidity_change_triggers_full_refetch() {
        let db = temp_db();
        let mut mock = MockProvider {
            state: MailboxState {
                uid_validity: 1,
                uid_next: 3,
                exists: 2,
            },
            messages: vec![msg(1), msg(2)],
        };
        let r0 = sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();
        assert_eq!(db.count_messages("acc", "acc:INBOX").unwrap(), 2);
        assert_eq!(r0.mailboxes[0].cancelled_ops, 0, "no reset, no cancels");

        // Server resets its UID space to a smaller, different set of messages.
        mock.state.uid_validity = 2;
        mock.state.uid_next = 11;
        mock.state.exists = 1;
        mock.messages = vec![msg(10)];
        let r = sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();
        assert_eq!(
            r.mailboxes[0].fetched, 1,
            "full refetch after UIDVALIDITY change"
        );
        // Old rows (uid 1,2) must be purged; only the new message remains.
        assert_eq!(
            r.mailboxes[0].total, 1,
            "stale rows purged on UIDVALIDITY reset"
        );
        assert!(db.get_message_detail("acc:INBOX:1").unwrap().is_none());
    }

    ///: a first-ever sync of a mailbox (no cursor yet) already
    /// has every UID's current flags from the descriptor fetch itself — the
    /// full `UID FETCH 1:* (UID FLAGS)` reconciliation pass must never run
    /// right after it.
    #[test]
    fn first_ever_sync_skips_the_redundant_uid_flags_scan() {
        let db = temp_db();
        let mut mock = CountingProvider {
            inner: MockProvider {
                state: MailboxState {
                    uid_validity: 1,
                    uid_next: 3,
                    exists: 2,
                },
                messages: vec![
                    RemoteMessage {
                        is_seen: true,
                        ..msg(1)
                    },
                    msg(2),
                ],
            },
            list_uid_flags_calls: 0,
        };
        let r = sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();
        assert_eq!(
            mock.list_uid_flags_calls, 0,
            "a from-scratch fetch already has every UID's current flags; \
             re-asking the server is redundant"
        );
        assert_eq!(r.mailboxes[0].fetched, 2);
        assert_eq!(db.count_messages("acc", "acc:INBOX").unwrap(), 2);
        // The flags the descriptor fetch itself carried must still have
        // landed, even with reconciliation skipped.
        assert!(
            db.get_message_detail("acc:INBOX:1")
                .unwrap()
                .unwrap()
                .header
                .is_read
        );
        assert!(
            !db.get_message_detail("acc:INBOX:2")
                .unwrap()
                .unwrap()
                .header
                .is_read
        );
    }

    ///, the edge case named in the task: no
    /// cursor recorded at all, but local rows already exist (an earlier
    /// first import that fetched rows and was interrupted before
    /// `persist_cursor` ever ran). Since PR #175 this is folded into the
    /// purge-and-refetch-from-scratch path, so it must skip the flags scan
    /// too — reconciling against a set that's about to be discarded would
    /// tell it nothing a fresh fetch doesn't already know.
    #[test]
    fn missing_cursor_with_stale_local_rows_still_skips_the_uid_flags_scan() {
        let db = temp_db();
        seed_account_mailbox(&db);
        // A stale row from an interrupted first import, with no cursor ever
        // written for it (the exact #2619 scenario) — uid 99 is not part of
        // the mock's server state below.
        db.upsert_message(&descriptor_message(99)).unwrap();
        assert!(db.get_sync_cursor("acc", "acc:INBOX").unwrap().is_none());

        let mut mock = CountingProvider {
            inner: MockProvider {
                state: MailboxState {
                    uid_validity: 1,
                    uid_next: 3,
                    exists: 2,
                },
                messages: vec![msg(1), msg(2)],
            },
            list_uid_flags_calls: 0,
        };
        let r = sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
            .unwrap();
        assert_eq!(
            mock.list_uid_flags_calls, 0,
            "no cursor + stale local rows takes the purge-and-refetch path \
             (#175); the refetched set already matches the server exactly"
        );
        assert!(
            db.get_message_detail("acc:INBOX:99").unwrap().is_none(),
            "the stale, cursor-less row must be purged, not left for \
             reconciliation to (fail to) resolve"
        );
        assert_eq!(r.mailboxes[0].total, 2);
    }

    #[test]
    fn uidvalidity_reset_cancels_stale_queued_ops() {
        use castellum_models::{DraftAddress, Mailbox, NewDraft};

        let db = temp_db();
        let mut mock = MockProvider {
            state: MailboxState {
                uid_validity: 1,
                uid_next: 3,
                exists: 2,
            },
            messages: vec![msg(1), msg(2)],
        };
        sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();
        db.set_account_write_enabled("acc", true).unwrap();

        // Queue ops against the soon-to-be-stale INBOX UID space...
        db.mark_read("acc:INBOX:1", true).unwrap();
        db.set_flagged("acc:INBOX:2", true).unwrap();
        // ...plus ops that must survive: one in another mailbox, one send.
        db.upsert_mailbox(&Mailbox {
            id: "acc:Sent".into(),
            account_id: "acc".into(),
            path: "Sent".into(),
            name: "Sent".into(),
            role: Some("sent".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.upsert_message(&castellum_models::NewMessage {
            id: "acc:Sent:1".into(),
            account_id: "acc".into(),
            mailbox_id: "acc:Sent".into(),
            provider_uid: Some(1),
            rfc822_message_id: Some("<s1@x>".into()),
            subject: Some("sent".into()),
            from_name: None,
            from_address: None,
            sent_at: Some(1),
            snippet: None,
            size_bytes: None,
            has_attachments: false,
            attachments: None,
            is_read: false,
            is_starred: false,
            addresses: vec![],
            text_body: None,
            html_body: None,
            sanitized_html: None,
            has_remote_content: false,
            references: vec![],
        })
        .unwrap();
        db.mark_read("acc:Sent:1", true).unwrap();
        let draft = db
            .save_draft(&NewDraft {
                id: None,
                account_id: "acc".into(),
                to: vec![DraftAddress {
                    name: None,
                    email: "b@x".into(),
                }],
                cc: vec![],
                bcc: vec![],
                subject: "s".into(),
                body_text: "t".into(),
                body_html: None,
                from_email: None,
                from_name: None,
                in_reply_to: None,
                references_header: None,
            })
            .unwrap();
        db.send_draft(draft).unwrap();
        assert_eq!(db.pending_operations("acc").unwrap().len(), 4);

        // UIDVALIDITY reset → purge + full refetch.
        mock.state.uid_validity = 2;
        mock.state.uid_next = 11;
        mock.state.exists = 1;
        mock.messages = vec![msg(10)];
        let r = sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();

        // The two INBOX ops are cancelled and surfaced in the report.
        assert_eq!(r.mailboxes[0].cancelled_ops, 2);
        let remaining = db.pending_operations("acc").unwrap();
        let kinds: Vec<&str> = remaining.iter().map(|o| o.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["mark_read", "send"],
            "other-mailbox op and send op survive the INBOX purge"
        );
        assert!(remaining.iter().all(|o| o.status == "queued"));
    }

    #[test]
    fn an_interrupted_first_import_purges_stale_rows_and_cancels_their_ops() {
        // A first import that never wrote a `sync_cursors` row (crashed, or
        // was killed mid-pass) leaves local rows under whatever UIDVALIDITY
        // the server had at the time — but nothing records what that was.
        // / DECISIONS.md (S30 addendum): the NEXT sync must
        // treat `cursor.is_none()` + existing local rows exactly like a
        // UIDVALIDITY reset, not silently adopt the new cursor over stale
        // rows whose UIDs may now name different messages.
        let db = temp_db();
        seed_account_mailbox(&db);
        db.upsert_message(&descriptor_message(1)).unwrap();
        db.mark_read("acc:INBOX:1", true).unwrap();
        assert_eq!(db.pending_operations("acc").unwrap().len(), 1);

        // No cursor was ever written for this mailbox.
        assert!(db.get_sync_cursor("acc", "acc:INBOX").unwrap().is_none());

        // The server reports a fresh UIDVALIDITY (it reset while we were
        // gone) with a different message set.
        let mut mock = MockProvider {
            state: MailboxState {
                uid_validity: 2,
                uid_next: 11,
                exists: 1,
            },
            messages: vec![msg(10)],
        };
        let r = sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();

        // The stale row's mark_read op is cancelled (reset reason), not
        // replayed against whatever message now occupies uid 1 under V2.
        assert_eq!(
            r.mailboxes[0].cancelled_ops, 1,
            "the stale first-import row's op is cancelled like a UIDVALIDITY reset"
        );
        assert!(
            db.pending_operations("acc").unwrap().is_empty(),
            "no queued op survives the purge"
        );

        // The stale row itself is gone; only the fresh import remains.
        assert!(db.get_message_detail("acc:INBOX:1").unwrap().is_none());
        let uids: Vec<i64> = db
            .list_local_uid_flags("acc", "acc:INBOX")
            .unwrap()
            .into_iter()
            .map(|(uid, _, _)| uid)
            .collect();
        assert_eq!(uids, vec![10], "the re-import fetched fresh descriptors");

        // The cursor now reflects the new UID space.
        let cursor = db.get_sync_cursor("acc", "acc:INBOX").unwrap().unwrap();
        assert_eq!(cursor.uid_validity, Some(2));
    }

    #[test]
    fn an_interrupted_first_import_resumed_with_the_same_uid_validity_reimports_cleanly() {
        // Same setup, but the server's UIDVALIDITY on resume happens to
        // match what the (unrecorded) stale rows were fetched under. Since
        // nothing records that old value, the fix purges unconditionally —
        // this test proves that path still converges to exactly one clean
        // copy per message, not a duplicate and not a loss.
        let db = temp_db();
        seed_account_mailbox(&db);
        db.upsert_message(&descriptor_message(1)).unwrap();
        db.mark_read("acc:INBOX:1", true).unwrap();
        assert!(db.get_sync_cursor("acc", "acc:INBOX").unwrap().is_none());

        let mut mock = MockProvider {
            state: MailboxState {
                uid_validity: 1,
                uid_next: 2,
                exists: 1,
            },
            messages: vec![msg(1)],
        };
        let r = sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();
        assert_eq!(r.mailboxes[0].total, 1, "no duplicate row for uid 1");

        let uids: Vec<i64> = db
            .list_local_uid_flags("acc", "acc:INBOX")
            .unwrap()
            .into_iter()
            .map(|(uid, _, _)| uid)
            .collect();
        assert_eq!(uids, vec![1]);

        // The row was actually re-fetched (not left as the stale descriptor
        // stub): `descriptor_message` sets subject "Descriptor 1", the mock's
        // raw message sets "Message 1".
        let detail = db.get_message_detail("acc:INBOX:1").unwrap().unwrap();
        assert_eq!(detail.header.subject.as_deref(), Some("Message 1"));

        // Even though the server's UIDVALIDITY happens to match what the
        // stale row was (unrecordedly) fetched under, the pre-existing
        // mark_read op against it is still cancelled: nothing on disk records
        // that the match is real rather than coincidental (a fresh
        // UIDVALIDITY that simply reused the same integer), so the purge
        // path — and its op cancellation — runs unconditionally.
        assert!(
            db.pending_operations("acc").unwrap().is_empty(),
            "the stale row's op does not survive an unverifiable UIDVALIDITY match"
        );

        let cursor = db.get_sync_cursor("acc", "acc:INBOX").unwrap().unwrap();
        assert_eq!(cursor.uid_validity, Some(1));
    }

    #[test]
    fn mailbox_filter_limits_scope() {
        let db = temp_db();
        let mut mock = MockProvider {
            state: MailboxState {
                uid_validity: 1,
                uid_next: 2,
                exists: 1,
            },
            messages: vec![msg(1)],
        };
        let filter = vec!["Sent".to_string()];
        let r =
            sync_with_provider(&db, "acc", "e@x", Some(&filter), &mut mock, "test-host").unwrap();
        assert!(r.mailboxes.is_empty(), "INBOX filtered out");
    }

    #[test]
    fn discovery_policy_persists_subscriptions_and_skips_large_archives_by_default() {
        struct DiscoveryMock {
            selected: Vec<String>,
        }
        impl MailProvider for DiscoveryMock {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![
                    RemoteMailbox {
                        path: "INBOX".into(),
                        name: "INBOX".into(),
                        role: Some("inbox".into()),
                        role_source: Some(MailboxRoleSource::SpecialUse),
                        delimiter: Some("/".into()),
                        subscribed: false,
                        selectable: true,
                    },
                    RemoteMailbox {
                        path: "Sent".into(),
                        name: "Sent".into(),
                        role: Some("sent".into()),
                        role_source: Some(MailboxRoleSource::SpecialUse),
                        delimiter: Some("/".into()),
                        subscribed: false,
                        selectable: true,
                    },
                    RemoteMailbox {
                        path: "Work.Client".into(),
                        name: "Client".into(),
                        role: None,
                        role_source: None,
                        delimiter: Some(".".into()),
                        subscribed: true,
                        selectable: true,
                    },
                    RemoteMailbox {
                        path: "[Gmail]/All Mail".into(),
                        name: "All Mail".into(),
                        role: Some("archive".into()),
                        role_source: Some(MailboxRoleSource::SpecialUse),
                        delimiter: Some("/".into()),
                        subscribed: false,
                        selectable: true,
                    },
                    RemoteMailbox {
                        path: "Old Projects".into(),
                        name: "Old Projects".into(),
                        role: None,
                        role_source: None,
                        delimiter: Some("/".into()),
                        subscribed: false,
                        selectable: true,
                    },
                    RemoteMailbox {
                        path: "Folders".into(),
                        name: "Folders".into(),
                        role: None,
                        role_source: None,
                        delimiter: Some("/".into()),
                        subscribed: false,
                        selectable: false,
                    },
                ])
            }
            fn select(&mut self, path: &str) -> Result<MailboxState, ProviderError> {
                self.selected.push(path.to_string());
                Ok(MailboxState {
                    uid_validity: 1,
                    uid_next: 2,
                    exists: 1,
                })
            }
            fn fetch_new(
                &mut self,
                _after_uid: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                Ok(vec![msg(1)])
            }
            fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
                Ok(vec![UidFlags {
                    keywords: None,
                    uid: 1,
                    is_seen: false,
                    is_flagged: false,
                }])
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }

        let db = temp_db();
        let mut mock = DiscoveryMock { selected: vec![] };
        let report = sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();

        assert_eq!(mock.selected, vec!["INBOX", "Sent", "Work.Client"]);
        let by_path = |p: &str| {
            report
                .mailboxes
                .iter()
                .find(|m| m.path == p)
                .unwrap_or_else(|| panic!("{p} missing from report"))
        };
        assert_eq!(
            by_path("[Gmail]/All Mail").skipped.as_deref(),
            Some("not in default sync set")
        );
        assert_eq!(
            by_path("Old Projects").skipped.as_deref(),
            Some("not in default sync set")
        );
        assert_eq!(
            by_path("Folders").skipped.as_deref(),
            Some("non-selectable")
        );

        let mailboxes = db.list_mailboxes("acc").unwrap();
        let work = mailboxes.iter().find(|m| m.path == "Work.Client").unwrap();
        assert_eq!(work.delimiter.as_deref(), Some("."));
        assert!(work.subscribed);
        assert!(work.sync_by_default);

        let all_mail = mailboxes
            .iter()
            .find(|m| m.path == "[Gmail]/All Mail")
            .unwrap();
        assert_eq!(all_mail.role.as_deref(), Some("archive"));
        assert!(all_mail.selectable);
        assert!(!all_mail.subscribed);
        assert!(!all_mail.sync_by_default);

        let parent = mailboxes.iter().find(|m| m.path == "Folders").unwrap();
        assert!(!parent.selectable);
        assert!(!parent.sync_by_default);
    }

    #[test]
    fn explicit_mailbox_filter_overrides_default_archive_gate() {
        struct ArchiveMock {
            selected: Vec<String>,
        }
        impl MailProvider for ArchiveMock {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![RemoteMailbox {
                    path: "[Gmail]/All Mail".into(),
                    name: "All Mail".into(),
                    role: Some("archive".into()),
                    role_source: Some(MailboxRoleSource::SpecialUse),
                    delimiter: Some("/".into()),
                    subscribed: false,
                    selectable: true,
                }])
            }
            fn select(&mut self, path: &str) -> Result<MailboxState, ProviderError> {
                self.selected.push(path.to_string());
                Ok(MailboxState {
                    uid_validity: 1,
                    uid_next: 2,
                    exists: 1,
                })
            }
            fn fetch_new(
                &mut self,
                _after_uid: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                Ok(vec![msg(1)])
            }
            fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
                Ok(vec![UidFlags {
                    keywords: None,
                    uid: 1,
                    is_seen: false,
                    is_flagged: false,
                }])
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }

        let db = temp_db();
        let mut mock = ArchiveMock { selected: vec![] };
        let filter = vec!["[Gmail]/All Mail".to_string()];
        let report =
            sync_with_provider(&db, "acc", "e@x", Some(&filter), &mut mock, "test-host").unwrap();

        assert_eq!(mock.selected, vec!["[Gmail]/All Mail"]);
        assert_eq!(report.mailboxes[0].fetched, 1);
        assert!(report.mailboxes[0].skipped.is_none());
        assert_eq!(db.count_messages("acc", "acc:[Gmail]/All Mail").unwrap(), 1);
    }

    #[test]
    fn provider_shape_fixtures_preserve_paths_and_default_sync_policy() {
        struct ShapeMock {
            mailboxes: Vec<RemoteMailbox>,
            selected: Vec<String>,
        }
        impl MailProvider for ShapeMock {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(self.mailboxes.clone())
            }
            fn select(&mut self, path: &str) -> Result<MailboxState, ProviderError> {
                self.selected.push(path.to_string());
                Ok(MailboxState {
                    uid_validity: 1,
                    uid_next: 2,
                    exists: 1,
                })
            }
            fn fetch_new(
                &mut self,
                _after_uid: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                Ok(vec![msg(1)])
            }
            fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
                Ok(vec![UidFlags {
                    keywords: None,
                    uid: 1,
                    is_seen: false,
                    is_flagged: false,
                }])
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }

        let fixtures = vec![
            (
                "gmail",
                vec![
                    rmb("INBOX", "INBOX", Some("inbox"), Some("/"), false, true),
                    rmb("[Gmail]", "[Gmail]", None, Some("/"), false, false),
                    rmb(
                        "[Gmail]/Sent Mail",
                        "Sent Mail",
                        Some("sent"),
                        Some("/"),
                        false,
                        true,
                    ),
                    rmb(
                        "[Gmail]/Drafts",
                        "Drafts",
                        Some("drafts"),
                        Some("/"),
                        false,
                        true,
                    ),
                    rmb(
                        "[Gmail]/Trash",
                        "Trash",
                        Some("trash"),
                        Some("/"),
                        false,
                        true,
                    ),
                    rmb(
                        "[Gmail]/All Mail",
                        "All Mail",
                        Some("archive"),
                        Some("/"),
                        false,
                        true,
                    ),
                    rmb("Labels/Client", "Client", None, Some("/"), true, true),
                ],
                vec![
                    "INBOX",
                    "[Gmail]/Sent Mail",
                    "[Gmail]/Drafts",
                    "[Gmail]/Trash",
                    "Labels/Client",
                ],
                vec![("[Gmail]/All Mail", false), ("Labels/Client", true)],
            ),
            (
                "icloud",
                vec![
                    rmb("INBOX", "INBOX", Some("inbox"), Some("/"), false, true),
                    rmb(
                        "Sent Messages",
                        "Sent Messages",
                        Some("sent"),
                        Some("/"),
                        false,
                        true,
                    ),
                    rmb(
                        "Deleted Messages",
                        "Deleted Messages",
                        Some("trash"),
                        Some("/"),
                        false,
                        true,
                    ),
                    rmb(
                        "Archive",
                        "Archive",
                        Some("archive"),
                        Some("/"),
                        false,
                        true,
                    ),
                    rmb("Junk", "Junk", Some("junk"), Some("/"), false, true),
                ],
                vec!["INBOX", "Sent Messages", "Deleted Messages", "Junk"],
                vec![("Archive", false), ("Deleted Messages", true)],
            ),
            (
                "fastmail",
                vec![
                    rmb("INBOX", "INBOX", Some("inbox"), Some("/"), false, true),
                    rmb("Sent", "Sent", Some("sent"), Some("/"), false, true),
                    rmb("Archive", "Archive", Some("archive"), Some("/"), true, true),
                    rmb("Lists/Rust", "Rust", None, Some("/"), true, true),
                ],
                vec!["INBOX", "Sent", "Archive", "Lists/Rust"],
                vec![("Archive", true), ("Lists/Rust", true)],
            ),
            (
                "dovecot",
                vec![
                    rmb("INBOX", "INBOX", Some("inbox"), Some("."), false, true),
                    rmb("Projects", "Projects", None, Some("."), false, false),
                    rmb("Projects.Client", "Client", None, Some("."), true, true),
                    rmb("Archive.2024", "2024", None, Some("."), false, true),
                    rmb("Sent", "Sent", Some("sent"), Some("."), false, true),
                ],
                vec!["INBOX", "Projects.Client", "Sent"],
                vec![("Projects.Client", true), ("Archive.2024", false)],
            ),
            (
                "exchange",
                vec![
                    rmb("INBOX", "INBOX", Some("inbox"), Some("/"), false, true),
                    rmb_special(
                        "Sent Items",
                        "Sent Items",
                        Some("sent"),
                        Some("/"),
                        false,
                        true,
                    ),
                    rmb_special(
                        "Deleted Items",
                        "Deleted Items",
                        Some("trash"),
                        Some("/"),
                        false,
                        true,
                    ),
                    rmb(
                        "Junk Email",
                        "Junk Email",
                        Some("junk"),
                        Some("/"),
                        false,
                        true,
                    ),
                    rmb(
                        "Archive",
                        "Archive",
                        Some("archive"),
                        Some("/"),
                        false,
                        true,
                    ),
                ],
                vec!["INBOX", "Sent Items", "Deleted Items", "Junk Email"],
                vec![("Archive", false), ("Sent Items", true)],
            ),
        ];

        for (name, mailboxes, expected_selected, checks) in fixtures {
            let db = temp_db();
            let mut mock = ShapeMock {
                mailboxes,
                selected: vec![],
            };
            let report = sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host")
                .unwrap_or_else(|e| panic!("{name}: sync failed: {e}"));

            assert_eq!(mock.selected, expected_selected, "{name}: selected set");
            assert_eq!(
                report.mailboxes.len(),
                mock.mailboxes.len(),
                "{name}: every discovered mailbox reported"
            );

            let stored = db.list_mailboxes("acc").unwrap();
            for (path, expected_default) in checks {
                let mailbox = stored
                    .iter()
                    .find(|m| m.path == path)
                    .unwrap_or_else(|| panic!("{name}: missing {path}"));
                assert_eq!(
                    mailbox.sync_by_default, expected_default,
                    "{name}: {path} sync_by_default"
                );
                assert!(
                    mailbox.delimiter.is_some(),
                    "{name}: {path} delimiter must be preserved"
                );
            }
        }
    }

    #[test]
    fn duplicate_special_use_roles_keep_one_canonical_write_target() {
        struct ConflictMock;
        impl MailProvider for ConflictMock {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![
                    rmb("INBOX", "INBOX", Some("inbox"), Some("/"), false, true),
                    rmb_special(
                        "Sent Items",
                        "Sent Items",
                        Some("sent"),
                        Some("/"),
                        true,
                        true,
                    ),
                    rmb("Sent", "Sent", Some("sent"), Some("/"), false, true),
                    rmb_special(
                        "Deleted Items",
                        "Deleted Items",
                        Some("trash"),
                        Some("/"),
                        true,
                        true,
                    ),
                    rmb("Trash", "Trash", Some("trash"), Some("/"), false, true),
                    rmb_special(
                        "[Gmail]/All Mail",
                        "All Mail",
                        Some("archive"),
                        Some("/"),
                        false,
                        true,
                    ),
                    rmb(
                        "Archive",
                        "Archive",
                        Some("archive"),
                        Some("/"),
                        false,
                        true,
                    ),
                ])
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
                _after_uid: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                Ok(vec![])
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }

        let db = temp_db();
        let mut mock = ConflictMock;
        sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();

        let stored = db.list_mailboxes("acc").unwrap();
        let role_paths = |role: &str| {
            stored
                .iter()
                .filter(|m| m.role.as_deref() == Some(role))
                .map(|m| m.path.as_str())
                .collect::<Vec<_>>()
        };

        assert_eq!(role_paths("sent"), vec!["Sent Items"]);
        assert_eq!(role_paths("trash"), vec!["Deleted Items"]);
        assert_eq!(role_paths("archive"), vec!["[Gmail]/All Mail"]);
        assert!(stored
            .iter()
            .any(|m| m.path == "Sent" && m.role.is_none() && !m.sync_by_default));
        assert!(stored
            .iter()
            .any(|m| m.path == "Trash" && m.role.is_none() && !m.sync_by_default));
        assert!(stored
            .iter()
            .any(|m| m.path == "Archive" && m.role.is_none() && !m.sync_by_default));
    }

    #[test]
    fn empty_mailbox_syncs_zero_without_fetch() {
        let db = temp_db();
        let mut mock = MockProvider {
            state: MailboxState {
                uid_validity: 1,
                uid_next: 1,
                exists: 0,
            },
            messages: vec![],
        };
        let r = sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();
        assert_eq!(r.mailboxes[0].fetched, 0);
        assert_eq!(r.mailboxes[0].total, 0);
    }

    #[test]
    fn sync_config_debug_redacts_password() {
        let cfg: ImapSyncConfig = serde_json::from_str(
            r#"{"account_id":"acc","email":"a@x","host":"imap.x","port":993,
                "username":"alice","password":"hunter2-super-secret"}"#,
        )
        .unwrap();
        let s = format!("{cfg:?}");
        assert!(!s.contains("hunter2-super-secret"), "password leaked: {s}");
        assert!(s.contains("<redacted>"));
        assert!(
            s.contains("imap.x") && s.contains("alice"),
            "other fields kept: {s}"
        );
    }

    #[test]
    fn sync_sanitizes_and_stores_html_bodies() {
        let db = temp_db();
        let raw = b"From: a@b\r\nTo: c@d\r\nSubject: xss\r\nMIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>hi</p><script>alert(1)</script><img src=\"https://t/p.gif\">".to_vec();
        let mut mock = MockProvider {
            state: MailboxState {
                uid_validity: 1,
                uid_next: 2,
                exists: 1,
            },
            messages: vec![RemoteMessage {
                uid: 1,
                raw,
                is_seen: false,
                is_flagged: false,
            }],
        };
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();

        let detail = db.get_message_detail("acc:INBOX:1").unwrap().unwrap();
        let html = detail.sanitized_html.expect("sanitized html stored");
        assert!(!html.contains("script"), "script tag must be stripped");
        assert!(html.contains("hi"), "safe content must be preserved");
        assert!(
            detail.has_remote_content,
            "remote image must set has_remote_content"
        );
    }

    /// S48: `sync_with_provider` computes and persists SPF/DKIM/DMARC trust
    /// signals scoped to the account's own configured `provider_host` — a
    /// different account's host must not retroactively trust the same header.
    #[test]
    fn sync_computes_and_stores_trust_signals_scoped_to_provider_host() {
        let db = temp_db();
        let raw = b"From: a@example.com\r\nTo: b@example.com\r\nSubject: s\r\n\
             Authentication-Results: mx.google.com; spf=pass; dkim=pass; dmarc=pass\r\n\
             Date: Tue, 1 Jul 2025 10:00:00 +0000\r\n\r\nbody\r\n"
            .to_vec();
        // #3472: a verdict is read only for a known-provider (IMAP host,
        // authserv-id) pair, so this fixture is a Gmail account.
        let mut mock = MockProvider {
            state: MailboxState {
                uid_validity: 1,
                uid_next: 2,
                exists: 1,
            },
            messages: vec![RemoteMessage {
                uid: 1,
                raw,
                is_seen: false,
                is_flagged: false,
            }],
        };
        sync_with_provider(
            &db,
            "acc",
            "test@local.test",
            None,
            &mut mock,
            "imap.gmail.com",
        )
        .unwrap();

        let detail = db.get_message_detail("acc:INBOX:1").unwrap().unwrap();
        assert_eq!(detail.trust.spf, TrustResult::Pass);
        assert_eq!(detail.trust.dkim, TrustResult::Pass);
        assert_eq!(detail.trust.dmarc, TrustResult::Pass);

        // A different account's host must not retroactively trust the same header.
        let db2 = temp_db();
        let raw2 = b"From: a@x\r\nTo: b@x\r\nSubject: s2\r\nAuthentication-Results: mail.example.com; spf=pass\r\n\r\nbody".to_vec();
        let mut mock2 = MockProvider {
            state: MailboxState {
                uid_validity: 1,
                uid_next: 2,
                exists: 1,
            },
            messages: vec![RemoteMessage {
                uid: 1,
                raw: raw2,
                is_seen: false,
                is_flagged: false,
            }],
        };
        sync_with_provider(
            &db2,
            "acc",
            "test@local.test",
            None,
            &mut mock2,
            "imap.unrelated-provider.test",
        )
        .unwrap();
        let detail2 = db2.get_message_detail("acc:INBOX:1").unwrap().unwrap();
        assert_eq!(detail2.trust.spf, TrustResult::None);
    }

    // --- RFC 4549 descriptor reconciliation (slice R1) ----------------------

    /// A remote \Seen / \Flagged change on an ALREADY-STORED message (no new UID)
    /// converges into the local flags via reconciliation, not via re-fetch.
    #[test]
    fn remote_flag_change_reconciles_existing_message() {
        let db = temp_db();
        let mut mock = MockProvider {
            state: MailboxState {
                uid_validity: 1,
                uid_next: 3,
                exists: 2,
            },
            messages: vec![msg(1), msg(2)],
        };
        sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();
        assert!(
            !db.get_message_detail("acc:INBOX:1")
                .unwrap()
                .unwrap()
                .header
                .is_read
        );

        // Server marks message 1 seen + flagged. No new messages arrive.
        mock.messages[0].is_seen = true;
        mock.messages[0].is_flagged = true;
        let r = sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();
        assert_eq!(r.mailboxes[0].fetched, 0, "no new messages fetched");
        assert_eq!(r.mailboxes[0].flags_reconciled, 1, "one message reconciled");
        assert_eq!(r.mailboxes[0].vanished, 0);

        let d1 = db.get_message_detail("acc:INBOX:1").unwrap().unwrap();
        assert!(d1.header.is_read, "remote \\Seen must reconcile locally");
        assert!(
            d1.header.is_starred,
            "remote \\Flagged must reconcile locally"
        );
        let d2 = db.get_message_detail("acc:INBOX:2").unwrap().unwrap();
        assert!(!d2.header.is_read, "untouched message stays unread");

        // Idempotent: a further sync with no change reconciles nothing.
        let r2 = sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();
        assert_eq!(r2.mailboxes[0].flags_reconciled, 0);
    }

    /// A message expunged/moved on the server (its UID no longer in the FLAGS
    /// snapshot) is purged locally.
    #[test]
    fn remote_expunge_purges_local_message() {
        let db = temp_db();
        let mut mock = MockProvider {
            state: MailboxState {
                uid_validity: 1,
                uid_next: 3,
                exists: 2,
            },
            messages: vec![msg(1), msg(2)],
        };
        sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();
        assert_eq!(db.count_messages("acc", "acc:INBOX").unwrap(), 2);

        // Server expunges message 1 (still one message left, so exists stays > 0).
        mock.messages.remove(0);
        mock.state.exists = 1;
        let r = sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();
        assert_eq!(r.mailboxes[0].vanished, 1, "expunged message purged");
        assert_eq!(r.mailboxes[0].total, 1);
        assert!(db.get_message_detail("acc:INBOX:1").unwrap().is_none());
        assert!(db.get_message_detail("acc:INBOX:2").unwrap().is_some());
    }

    /// Flag change and expunge reconciled in the same sync run.
    #[test]
    fn combined_flag_change_and_expunge() {
        let db = temp_db();
        let mut mock = MockProvider {
            state: MailboxState {
                uid_validity: 1,
                uid_next: 4,
                exists: 3,
            },
            messages: vec![msg(1), msg(2), msg(3)],
        };
        sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();

        // msg 2 read on server; msg 3 expunged.
        mock.messages[1].is_seen = true;
        mock.messages.remove(2);
        mock.state.exists = 2;
        let r = sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();
        assert_eq!(r.mailboxes[0].flags_reconciled, 1);
        assert_eq!(r.mailboxes[0].vanished, 1);
        assert!(
            db.get_message_detail("acc:INBOX:2")
                .unwrap()
                .unwrap()
                .header
                .is_read
        );
        assert!(db.get_message_detail("acc:INBOX:3").unwrap().is_none());
    }

    /// When the server empties a mailbox (exists == 0), stale local rows are
    /// purged so local converges — the emptied-mailbox path reconciles too.
    #[test]
    fn server_emptied_mailbox_purges_local_rows() {
        let db = temp_db();
        let mut mock = MockProvider {
            state: MailboxState {
                uid_validity: 1,
                uid_next: 3,
                exists: 2,
            },
            messages: vec![msg(1), msg(2)],
        };
        sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();
        assert_eq!(db.count_messages("acc", "acc:INBOX").unwrap(), 2);

        // Everything expunged; mailbox now empty.
        mock.messages.clear();
        mock.state.exists = 0;
        let r = sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();
        assert_eq!(r.mailboxes[0].vanished, 2);
        assert_eq!(r.mailboxes[0].total, 0);
    }

    /// A message with a queued per-message flag op is the user's pending truth:
    /// reconciliation must NOT overwrite it with the (stale) server state, and
    /// must NOT purge it if it also went missing server-side.
    #[test]
    fn pending_flag_op_is_not_clobbered_by_reconciliation() {
        let db = temp_db();
        let mut mock = MockProvider {
            state: MailboxState {
                uid_validity: 1,
                uid_next: 3,
                exists: 2,
            },
            messages: vec![msg(1), msg(2)],
        };
        sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();
        db.set_account_write_enabled("acc", true).unwrap();

        // User marks message 1 read locally → queues a mark_read op. The server
        // still shows it unread (op not replayed yet).
        db.mark_read("acc:INBOX:1", true).unwrap();
        assert!(
            db.get_message_detail("acc:INBOX:1")
                .unwrap()
                .unwrap()
                .header
                .is_read
        );

        let r = sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();
        assert_eq!(
            r.mailboxes[0].flags_reconciled, 0,
            "reconciliation must skip the message with a pending op"
        );
        assert!(
            db.get_message_detail("acc:INBOX:1")
                .unwrap()
                .unwrap()
                .header
                .is_read,
            "local pending mark_read must survive reconciliation"
        );
        // And the queued op is still there to be replayed.
        assert!(db
            .pending_operations("acc")
            .unwrap()
            .iter()
            .any(|o| o.kind == "mark_read"));
    }

    /// A UIDVALIDITY reset that lands the mailbox empty must cancel stale
    /// UID-bound ops (not just purge rows) — UIDs can be reused in the new space,
    /// so a leftover mark_read/flag would target the wrong message.
    #[test]
    fn emptied_with_uidvalidity_change_cancels_stale_ops() {
        let db = temp_db();
        let mut mock = MockProvider {
            state: MailboxState {
                uid_validity: 1,
                uid_next: 3,
                exists: 2,
            },
            messages: vec![msg(1), msg(2)],
        };
        sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();
        db.set_account_write_enabled("acc", true).unwrap();

        // Queue a per-message op against the soon-to-be-stale UID space.
        db.mark_read("acc:INBOX:1", true).unwrap();
        assert_eq!(db.pending_operations("acc").unwrap().len(), 1);

        // Server resets its UID space and ends up empty.
        mock.state.uid_validity = 2;
        mock.state.uid_next = 1;
        mock.state.exists = 0;
        mock.messages.clear();
        let r = sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();
        assert_eq!(
            r.mailboxes[0].cancelled_ops, 1,
            "stale op cancelled on reset-to-empty"
        );
        assert_eq!(r.mailboxes[0].total, 0, "all rows purged on reset");
        assert!(
            db.pending_operations("acc").unwrap().is_empty(),
            "the stale op must no longer be pending"
        );
        assert!(db.get_message_detail("acc:INBOX:1").unwrap().is_none());
    }

    /// A non-selectable (`\Noselect`) folder and a folder that errors on SELECT
    /// must NOT abort the whole account sync: every other mailbox still syncs, and
    /// each problem folder is recorded in its report entry.
    #[test]
    fn non_selectable_and_failing_select_do_not_abort_sync() {
        /// Four folders: INBOX (ok, 1 msg), a `\Noselect` container, a folder whose
        /// SELECT fails, and — crucially AFTER the failing one — a healthy `Sent`,
        /// so the test proves iteration continues PAST the error. `select` errors
        /// for "Broken" only.
        struct MultiMailboxMock;
        impl MailProvider for MultiMailboxMock {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![
                    RemoteMailbox {
                        path: "INBOX".into(),
                        name: "INBOX".into(),
                        role: Some("inbox".into()),
                        role_source: Some(MailboxRoleSource::SpecialUse),
                        delimiter: Some("/".into()),
                        subscribed: true,
                        selectable: true,
                    },
                    RemoteMailbox {
                        path: "Archive".into(),
                        name: "Archive".into(),
                        role: Some("archive".into()),
                        role_source: Some(MailboxRoleSource::Heuristic),
                        delimiter: Some("/".into()),
                        subscribed: false,
                        selectable: false, // container-only parent
                    },
                    RemoteMailbox {
                        path: "Broken".into(),
                        name: "Broken".into(),
                        role: None,
                        role_source: None,
                        delimiter: Some("/".into()),
                        subscribed: true,
                        selectable: true,
                    },
                    RemoteMailbox {
                        path: "Sent".into(),
                        name: "Sent".into(),
                        role: Some("sent".into()),
                        role_source: Some(MailboxRoleSource::SpecialUse),
                        delimiter: Some("/".into()),
                        subscribed: true,
                        selectable: true,
                    },
                ])
            }
            fn select(&mut self, path: &str) -> Result<MailboxState, ProviderError> {
                if path == "Broken" {
                    return Err(ProviderError::Protocol("SELECT denied".into()));
                }
                Ok(MailboxState {
                    uid_validity: 1,
                    uid_next: 2,
                    exists: 1,
                })
            }
            fn fetch_new(
                &mut self,
                _after: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                Ok(vec![msg(1)])
            }
            fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
                Ok(vec![UidFlags {
                    keywords: None,
                    uid: 1,
                    is_seen: false,
                    is_flagged: false,
                }])
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }

        let db = temp_db();
        let mut mock = MultiMailboxMock;
        let report = sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();

        let by_path = |p: &str| {
            report
                .mailboxes
                .iter()
                .find(|m| m.path == p)
                .unwrap_or_else(|| panic!("{p} missing from report"))
        };
        // INBOX synced normally.
        assert_eq!(by_path("INBOX").fetched, 1);
        assert!(by_path("INBOX").error.is_none());
        // The \Noselect container is recorded, not fetched, and not an error.
        assert_eq!(by_path("Archive").fetched, 0);
        assert!(by_path("Archive").error.is_none());
        // The failing SELECT is recorded as an error but did not abort the run.
        assert_eq!(by_path("Broken").fetched, 0);
        assert!(by_path("Broken")
            .error
            .as_deref()
            .unwrap()
            .contains("SELECT denied"));
        // The folder AFTER the failing one still synced — direct proof the loop
        // continued past the error rather than aborting.
        assert_eq!(by_path("Sent").fetched, 1);
        assert!(by_path("Sent").error.is_none());

        // All mailboxes were upserted (hierarchy preserved), and both healthy
        // folders' messages actually landed.
        assert_eq!(db.count_messages("acc", "acc:INBOX").unwrap(), 1);
        assert_eq!(db.count_messages("acc", "acc:Sent").unwrap(), 1);
        assert_eq!(report.mailboxes.len(), 4);
    }

    /// A pending `mark_all_read` locks only the READ flag: a server-side star
    /// change still reconciles (the two flags are orthogonal).
    #[test]
    fn mark_all_read_pending_still_reconciles_star() {
        let db = temp_db();
        let mut mock = MockProvider {
            state: MailboxState {
                uid_validity: 1,
                uid_next: 3,
                exists: 2,
            },
            messages: vec![msg(1), msg(2)],
        };
        sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();
        db.set_account_write_enabled("acc", true).unwrap();

        // Bulk mark read locally → queues mark_all_read; both now read locally.
        db.mark_all_read("acc", "acc:INBOX").unwrap();
        assert!(
            db.get_message_detail("acc:INBOX:1")
                .unwrap()
                .unwrap()
                .header
                .is_read
        );

        // Server stars message 1 (still unseen server-side).
        mock.messages[0].is_flagged = true;
        let r = sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();
        assert_eq!(
            r.mailboxes[0].flags_reconciled, 1,
            "star reconciles despite the pending mark_all_read"
        );
        let d = db.get_message_detail("acc:INBOX:1").unwrap().unwrap();
        assert!(d.header.is_starred, "server star must be applied");
        assert!(
            d.header.is_read,
            "local pending mark_all_read (read=true) must be preserved"
        );
    }

    // --- durable sync diagnostics (migration 0016) ---------------------------

    #[test]
    fn sync_records_mailbox_attempt_and_success() {
        let db = temp_db();
        let mut mock = MockProvider {
            state: MailboxState {
                uid_validity: 7,
                uid_next: 2,
                exists: 1,
            },
            messages: vec![msg(1)],
        };
        sync_with_provider(&db, "acc", "e@x", None, &mut mock, "test-host").unwrap();

        let states = db.list_mailbox_sync_states("acc").unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].mailbox_id, "acc:INBOX");
        assert!(states[0].last_attempt_at.is_some());
        assert!(states[0].last_success_at.is_some());
        assert_eq!(states[0].last_error_kind, None);
        assert_eq!(states[0].consecutive_failures, 0);

        // Account-level state is owned by sync_imap_json, not the engine.
        assert!(db.get_account_sync_state("acc").unwrap().is_none());
    }

    #[test]
    fn select_failure_records_redacted_kind_and_phase_then_success_resets() {
        const SENTINEL: &str = "NO [AUTHENTICATIONFAILED] user=alice pass=hunter2-SENTINEL";

        /// SELECT fails with server text that must never be persisted.
        struct SelectFailMock;
        impl MailProvider for SelectFailMock {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![rmb("INBOX", "INBOX", Some("inbox"), None, true, true)])
            }
            fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
                Err(ProviderError::Auth(SENTINEL.into()))
            }
            fn fetch_new(&mut self, _: Option<i64>) -> Result<Vec<RemoteMessage>, ProviderError> {
                unreachable!("select failed")
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }

        let db = temp_db();
        let report =
            sync_with_provider(&db, "acc", "e@x", None, &mut SelectFailMock, "test-host").unwrap();
        assert!(report.mailboxes[0].error.is_some());

        let states = db.list_mailbox_sync_states("acc").unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].last_error_kind.as_deref(), Some("auth"));
        assert_eq!(
            states[0].last_error_redacted.as_deref(),
            Some("authentication failed")
        );
        assert_eq!(states[0].last_error_phase.as_deref(), Some("select"));
        assert_eq!(states[0].consecutive_failures, 1);
        assert!(states[0].last_success_at.is_none());
        let dump = format!("{states:?}");
        assert!(
            !dump.contains("SENTINEL") && !dump.contains("hunter2"),
            "raw server text persisted: {dump}"
        );

        // A later failure increments; a success clears the error state.
        let _ =
            sync_with_provider(&db, "acc", "e@x", None, &mut SelectFailMock, "test-host").unwrap();
        assert_eq!(
            db.list_mailbox_sync_states("acc").unwrap()[0].consecutive_failures,
            2
        );
        let mut ok = MockProvider {
            state: MailboxState {
                uid_validity: 7,
                uid_next: 1,
                exists: 0,
            },
            messages: vec![],
        };
        sync_with_provider(&db, "acc", "e@x", None, &mut ok, "test-host").unwrap();
        let states = db.list_mailbox_sync_states("acc").unwrap();
        assert_eq!(states[0].consecutive_failures, 0);
        assert_eq!(states[0].last_error_kind, None);
        assert!(states[0].last_success_at.is_some());
    }

    #[test]
    fn skipped_mailboxes_record_no_attempt() {
        /// One container-only folder and one non-default folder; both skipped.
        struct SkippedMock;
        impl MailProvider for SkippedMock {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![
                    rmb("Parent", "Parent", None, Some("/"), false, false),
                    rmb(
                        "Archive",
                        "Archive",
                        Some("archive"),
                        Some("/"),
                        false,
                        true,
                    ),
                ])
            }
            fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
                unreachable!("skipped mailboxes must not be selected")
            }
            fn fetch_new(&mut self, _: Option<i64>) -> Result<Vec<RemoteMessage>, ProviderError> {
                unreachable!()
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }

        let db = temp_db();
        let report =
            sync_with_provider(&db, "acc", "e@x", None, &mut SkippedMock, "test-host").unwrap();
        assert!(report.mailboxes.iter().all(|m| m.skipped.is_some()));
        assert!(
            db.list_mailbox_sync_states("acc").unwrap().is_empty(),
            "skipped mailboxes must not record sync attempts"
        );
    }

    #[test]
    fn sync_imap_json_records_account_connect_failure_without_account_row() {
        let db = temp_db();
        // 127.0.0.1:1 refuses immediately: a fast, deterministic connect error.
        let config = r#"{"account_id":"acc-new","email":"t@local.test",
            "host":"127.0.0.1","port":1,"username":"u","password":"p","use_tls":false}"#;
        assert!(sync_imap_json(&db, config).is_err());

        let state = db.get_account_sync_state("acc-new").unwrap().unwrap();
        assert!(state.last_attempt_at.is_some());
        assert_eq!(state.last_error_kind.as_deref(), Some("network"));
        assert_eq!(
            state.last_error_redacted.as_deref(),
            Some("network connection failed")
        );
        assert_eq!(state.last_error_phase.as_deref(), Some("connect"));
        assert_eq!(state.consecutive_failures, 1);
        // The failure precedes account creation — recording must not need it.
        assert!(db.list_accounts().unwrap().is_empty());
    }

    #[test]
    fn save_account_without_sync_creates_the_account_row_after_a_connect_failure() {
        // Reproduces the "Save anyway" bug: a first-ever connect fails (e.g.
        // IMAP not yet enabled on the mail provider), which records a
        // diagnostic failure but — as the test above documents — leaves
        // `accounts` empty. Without `save_account_without_sync_json`, the
        // Keychain held valid credentials but `list_accounts` (what the UI
        // reads) never saw the account.
        let db = temp_db();
        let config = r#"{"account_id":"acc-save-anyway","email":"t@local.test",
            "host":"127.0.0.1","port":1,"username":"u","password":"p","use_tls":false}"#;
        assert!(sync_imap_json(&db, config).is_err());
        assert!(db.list_accounts().unwrap().is_empty());

        save_account_without_sync_json(&db, config).expect("save anyway must succeed");

        let accounts = db.list_accounts().unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, "acc-save-anyway");
        assert_eq!(accounts[0].email, "t@local.test");

        // The failure the connect attempt already recorded must be left
        // exactly as it was — "Save anyway" reuses it rather than resetting
        // or duplicating it.
        let state = db
            .get_account_sync_state("acc-save-anyway")
            .unwrap()
            .unwrap();
        assert_eq!(state.consecutive_failures, 1);
        assert_eq!(state.last_error_kind.as_deref(), Some("network"));
    }

    #[test]
    fn save_account_without_sync_is_idempotent_and_updates_email() {
        // A second "Save anyway" (e.g. the user edited the email and retried)
        // must update the existing row, not create a duplicate — same
        // upsert-by-id contract `sync_with_provider` relies on.
        let db = temp_db();
        let config_v1 = r#"{"account_id":"acc-retry","email":"old@local.test",
            "host":"127.0.0.1","port":1,"username":"u","password":"p","use_tls":false}"#;
        let config_v2 = r#"{"account_id":"acc-retry","email":"new@local.test",
            "host":"127.0.0.1","port":1,"username":"u","password":"p","use_tls":false}"#;
        save_account_without_sync_json(&db, config_v1).unwrap();
        save_account_without_sync_json(&db, config_v2).unwrap();

        let accounts = db.list_accounts().unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].email, "new@local.test");
    }

    #[test]
    fn save_account_without_sync_persists_display_name_and_falls_back_when_blank() {
        // S70: the setup sheet's Display name must reach accounts.display_name
        // (and so `list_accounts_json`, which the sidebar reads). Blank or
        // whitespace-only means "no name" so the UI falls back to the address.
        let db = temp_db();
        let named = r#"{"account_id":"acc-dn","email":"n@local.test",
            "display_name":"  Núria Pujol, Ràdio \"Cat\"  ",
            "host":"127.0.0.1","port":1,"username":"u","password":"p","use_tls":false}"#;
        save_account_without_sync_json(&db, named).unwrap();
        let accounts = db.list_accounts().unwrap();
        assert_eq!(
            accounts[0].display_name.as_deref(),
            Some("Núria Pujol, Ràdio \"Cat\"")
        );
        let json = serde_json::to_string(&accounts).unwrap();
        assert!(json.contains("\"display_name\":\"Núria"), "{json}");

        // Editing the account to a blank name clears it.
        let blank = r#"{"account_id":"acc-dn","email":"n@local.test","display_name":"   ",
            "host":"127.0.0.1","port":1,"username":"u","password":"p","use_tls":false}"#;
        save_account_without_sync_json(&db, blank).unwrap();
        assert_eq!(db.list_accounts().unwrap()[0].display_name, None);

        // A pre-S70 Keychain config with no display_name key still decodes.
        let old = r#"{"account_id":"acc-old","email":"t@local.test",
            "host":"127.0.0.1","port":1,"username":"u","password":"p","use_tls":false}"#;
        save_account_without_sync_json(&db, old).unwrap();
        let accounts = db.list_accounts().unwrap();
        let old_row = accounts.iter().find(|a| a.id == "acc-old").unwrap();
        assert_eq!(old_row.display_name, None);
    }

    #[test]
    fn provider_sync_keeps_the_stored_display_name() {
        // The sync upsert used to overwrite display_name with NULL on every
        // pass, so a saved name vanished at the next sync.
        let db = temp_db();
        let named = r#"{"account_id":"acc","email":"test@local.test","display_name":"Marc R",
            "host":"127.0.0.1","port":1,"username":"u","password":"p","use_tls":false}"#;
        save_account_without_sync_json(&db, named).unwrap();
        let mut mock = MockProvider {
            state: MailboxState {
                uid_validity: 1,
                uid_next: 1,
                exists: 0,
            },
            messages: vec![],
        };
        sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host").unwrap();
        assert_eq!(
            db.list_accounts().unwrap()[0].display_name.as_deref(),
            Some("Marc R")
        );
    }

    fn sync_cfg(account_id: &str) -> ImapSyncConfig {
        serde_json::from_str(&format!(
            r#"{{"account_id":"{account_id}","email":"t@local.test",
                "host":"127.0.0.1","port":1,"username":"u","password":"p","use_tls":false}}"#
        ))
        .unwrap()
    }

    /// A panic inside connect (LOGIN/CAPABILITY are parsed there) must be
    /// recorded against the account, not unwind past the match. Before the
    /// connect was guarded, this test hit the `catch_unwind` at the FFI
    /// boundary instead and the account was left showing an attempt with no
    /// error, `consecutive_failures` stuck at 0 — diagnostics blind to exactly
    /// the crash class they exist for.
    #[test]
    fn a_panic_in_connect_is_recorded_as_a_connect_failure() {
        let db = temp_db();
        let result = sync_imap_json_with(&db, sync_cfg("acc-panic"), |_cfg| {
            // Stands in for imap 2.4.1 `read_response_onto`'s tag assert_eq!.
            panic!("assertion `left == right` failed: server bytes 0x41 0x42");
            #[allow(unreachable_code)]
            Ok(MockProvider {
                state: MailboxState {
                    uid_validity: 1,
                    uid_next: 1,
                    exists: 0,
                },
                messages: vec![],
            })
        });
        assert!(
            result.is_err(),
            "a panic in connect must surface as an error"
        );

        let state = db.get_account_sync_state("acc-panic").unwrap().unwrap();
        assert_eq!(state.last_error_phase.as_deref(), Some("connect"));
        assert_eq!(state.last_error_kind.as_deref(), Some("protocol"));
        assert_eq!(state.consecutive_failures, 1);
        assert!(
            !state
                .last_error_redacted
                .as_deref()
                .unwrap_or_default()
                .contains("0x41"),
            "the recorded summary must never carry the panic payload"
        );
    }

    /// A parser crash mid-sync abandons the session, so nothing after it can
    /// sync — the pass must NOT be recorded as an account success. It returns
    /// `Ok` (per-mailbox failures are deliberately non-fatal), and recording
    /// success there would clear `last_error_*` and reset
    /// `consecutive_failures` to 0, leaving the account reading healthy after
    /// a crash.
    #[test]
    fn a_crashed_parser_mid_sync_is_an_account_failure_not_a_success() {
        /// Two mailboxes; `select` always panics, as `parse_mailbox`'s
        /// `unreachable!()` does on an untagged `* NO` in a SELECT reply.
        struct SelectPanics {
            selects: usize,
        }
        impl MailProvider for SelectPanics {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![
                    rmb("INBOX", "INBOX", Some("inbox"), Some("/"), true, true),
                    rmb("Archive", "Archive", Some("archive"), Some("/"), true, true),
                ])
            }
            fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
                self.selects += 1;
                panic!("internal error: entered unreachable code: server bytes 0x41");
            }
            fn fetch_new(&mut self, _: Option<i64>) -> Result<Vec<RemoteMessage>, ProviderError> {
                unreachable!("never reached: select crashes first")
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }

        let db = temp_db();
        let json = sync_imap_json_with(&db, sync_cfg("acc-crash"), |_cfg| {
            Ok(SelectPanics { selects: 0 })
        })
        .expect("per-mailbox failures keep the pass Ok");
        assert!(json.contains("\"mailboxes\""));

        let state = db.get_account_sync_state("acc-crash").unwrap().unwrap();
        assert!(
            state.last_success_at.is_none(),
            "a run in which the parser crashed must not be recorded as a success"
        );
        assert_eq!(state.last_error_phase.as_deref(), Some("sync"));
        assert_eq!(state.last_error_kind.as_deref(), Some("protocol"));
        assert_eq!(state.consecutive_failures, 1);

        // The abandoned session short-circuits the remaining mailboxes instead
        // of recording an identical failure for each.
        let mailbox_states = db.list_mailbox_sync_states("acc-crash").unwrap();
        assert_eq!(
            mailbox_states.len(),
            1,
            "the loop must stop once the session is abandoned, got {mailbox_states:?}"
        );
    }

    mod initial_history_window {
        //! S62: `initial_sync_since` limits a mailbox's FIRST import to recent
        //! mail; later passes resume from the cursor and never back-fill.
        use super::*;

        /// `MockProvider` plus a scripted `first_uid_since` answer, recording
        /// the date it was asked for.
        struct WindowMock {
            inner: MockProvider,
            since: Option<Result<Option<i64>, ProviderError>>,
            asked: Vec<String>,
        }

        impl MailProvider for WindowMock {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                self.inner.list_mailboxes()
            }
            fn select(&mut self, path: &str) -> Result<MailboxState, ProviderError> {
                self.inner.select(path)
            }
            fn fetch_new(
                &mut self,
                after_uid: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                self.inner.fetch_new(after_uid)
            }
            fn first_uid_since(&mut self, date: &str) -> Result<Option<i64>, ProviderError> {
                self.asked.push(date.to_string());
                self.since
                    .take()
                    .unwrap_or(Err(ProviderError::Unsupported("unscripted".into())))
            }
            fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
                self.inner.list_uid_flags()
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }

        fn mock(since: Result<Option<i64>, ProviderError>) -> WindowMock {
            WindowMock {
                inner: MockProvider {
                    state: MailboxState {
                        uid_validity: 7,
                        uid_next: 11,
                        exists: 10,
                    },
                    messages: (1..=10).map(msg).collect(),
                },
                since: Some(since),
                asked: Vec::new(),
            }
        }

        fn sync(db: &Database, p: &mut WindowMock, since: Option<&str>) -> SyncReport {
            sync_with_provider_scoped(db, "acc", "a@x", None, since, p, "imap.x").unwrap()
        }

        #[test]
        fn first_sync_imports_only_the_window_and_later_syncs_never_backfill() {
            let db = temp_db();
            let mut p = mock(Ok(Some(8)));
            let r = sync(&db, &mut p, Some("1-Jun-2026"));
            assert_eq!(p.asked, vec!["1-Jun-2026".to_string()]);
            assert_eq!(r.mailboxes[0].fetched, 3, "UIDs 8..=10 only");
            assert_eq!(r.mailboxes[0].fetched_uid_min, Some(8));
            assert_eq!(r.mailboxes[0].total, 3);

            // Next pass: a cursor exists, so the window is not consulted and
            // only genuinely new mail (UID 11) arrives — no back-fill of 1..7.
            p.inner.messages.push(msg(11));
            p.inner.state.uid_next = 12;
            p.inner.state.exists = 11;
            p.since = Some(Ok(Some(1)));
            let r2 = sync(&db, &mut p, Some("1-Jun-2026"));
            assert_eq!(p.asked.len(), 1, "window only applies to the first import");
            assert_eq!(r2.mailboxes[0].fetched, 1);
            assert_eq!(r2.mailboxes[0].total, 4);
        }

        #[test]
        fn nothing_in_the_window_imports_nothing_but_still_sets_the_cursor() {
            let db = temp_db();
            let mut p = mock(Ok(None));
            let r = sync(&db, &mut p, Some("1-Jun-2026"));
            assert_eq!(r.mailboxes[0].fetched, 0);
            assert_eq!(r.mailboxes[0].total, 0);
            assert!(r.mailboxes[0].error.is_none());
            // Cursor advanced to UIDNEXT, so the next pass sees only new mail.
            p.inner.messages.push(msg(11));
            p.inner.state.uid_next = 12;
            p.inner.state.exists = 11;
            let r2 = sync(&db, &mut p, Some("1-Jun-2026"));
            assert_eq!(r2.mailboxes[0].fetched, 1);
            assert_eq!(r2.mailboxes[0].fetched_uid_min, Some(11));
        }

        #[test]
        fn no_window_means_all_mail_and_search_is_never_issued() {
            let db = temp_db();
            let mut p = mock(Ok(Some(8)));
            let r = sync(&db, &mut p, None);
            assert!(p.asked.is_empty());
            assert_eq!(r.mailboxes[0].fetched, 10);
        }

        #[test]
        fn an_unsupported_search_falls_back_to_the_full_import() {
            let db = temp_db();
            let mut p = mock(Err(ProviderError::Unsupported("no SEARCH".into())));
            let r = sync(&db, &mut p, Some("1-Jun-2026"));
            assert_eq!(r.mailboxes[0].fetched, 10);
        }

        #[test]
        fn a_search_error_fails_the_mailbox_instead_of_downloading_everything() {
            let db = temp_db();
            let mut p = mock(Err(ProviderError::Protocol("BAD".into())));
            let r = sync(&db, &mut p, Some("1-Jun-2026"));
            assert_eq!(r.mailboxes[0].fetched, 0);
            assert!(r.mailboxes[0].error.is_some());
            // No cursor was written, so the next pass retries the window.
            p.since = Some(Ok(Some(9)));
            let r2 = sync(&db, &mut p, Some("1-Jun-2026"));
            assert_eq!(r2.mailboxes[0].fetched, 2);
        }

        #[test]
        fn a_uidvalidity_reset_reimports_everything_not_just_the_window() {
            // Window chosen at setup, then (via the cursor) all 10 messages
            // arrive over time. A later UIDVALIDITY reset purges the local
            // rows; re-applying the window would silently lose 1..7.
            let db = temp_db();
            let mut p = mock(Ok(Some(8)));
            sync(&db, &mut p, Some("1-Jun-2026"));
            let r = sync(&db, &mut p, None); // steady state: cursor present
            assert_eq!(r.mailboxes[0].total, 3);

            p.inner.state.uid_validity = 8; // server reset its UID space
            p.since = Some(Ok(Some(8)));
            let r = sync(&db, &mut p, Some("1-Jun-2026"));
            assert_eq!(p.asked.len(), 1, "no window lookup after a reset");
            assert_eq!(r.mailboxes[0].fetched, 10, "full re-import after reset");
            assert_eq!(r.mailboxes[0].total, 10);
        }

        #[test]
        fn imap_since_date_is_strict() {
            assert_eq!(imap_since_date("2026-06-01").unwrap(), "1-Jun-2026");
            assert_eq!(imap_since_date("2025-12-31").unwrap(), "31-Dec-2025");
            for bad in [
                "2026-6-1",
                "2026-13-01",
                "2026-00-10",
                "2026-01-32",
                "1969-01-01",
                "2026-01-01 OR ALL",
                "26-01-01",
                "",
            ] {
                assert!(imap_since_date(bad).is_err(), "{bad:?} must be rejected");
            }
        }

        #[test]
        fn a_malformed_window_is_rejected_before_connecting() {
            let db = temp_db();
            let cfg: ImapSyncConfig = serde_json::from_str(
                r#"{"account_id":"acc","email":"a@x","host":"h","port":993,
                    "username":"u","password":"p","initial_sync_since":"last month"}"#,
            )
            .unwrap();
            let err = sync_imap_json_with(&db, cfg, |_| -> Result<MockProvider, _> {
                panic!("must not connect with a malformed window")
            })
            .unwrap_err();
            assert!(format!("{err:?}").contains("initial_sync_since"));
        }
    }

    /// follow-up review, missing test: unticking a role folder
    /// (`apply_mailbox_selection`, castellum-db) purges its cursor but keeps
    /// its row; re-ticking it must therefore be a genuinely FRESH first
    /// import on the next sync — no cursor means `first_ever_import` is
    /// true — honouring `initial_sync_since` exactly like any other new
    /// mailbox, never silently resuming or back-filling.
    #[test]
    fn a_re_ticked_role_folder_is_a_fresh_first_import_honouring_the_window() {
        use std::cell::RefCell;

        /// Two mailboxes (INBOX, Trash), tracking which one `select` last
        /// chose so `fetch_new`/`first_uid_since`/`list_uid_flags` (none of
        /// which take a path — real IMAP SELECT is stateful, so the mock is
        /// too) answer for the right one. INBOX already has a cursor (not
        /// under test here); Trash does not, after the untick below purges
        /// it — that's the whole point.
        struct RoleReselectMock {
            current: RefCell<String>,
        }
        impl MailProvider for RoleReselectMock {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![
                    rmb("INBOX", "INBOX", Some("inbox"), Some("/"), true, true),
                    rmb("Trash", "Trash", Some("trash"), Some("/"), true, true),
                ])
            }
            fn select(&mut self, path: &str) -> Result<MailboxState, ProviderError> {
                *self.current.borrow_mut() = path.to_string();
                Ok(match path {
                    "INBOX" => MailboxState {
                        uid_validity: 1,
                        uid_next: 2,
                        exists: 1,
                    },
                    "Trash" => MailboxState {
                        uid_validity: 5,
                        uid_next: 11,
                        exists: 10,
                    },
                    other => panic!("unexpected mailbox {other}"),
                })
            }
            fn fetch_new(
                &mut self,
                after_uid: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                match self.current.borrow().as_str() {
                    "INBOX" => Ok(vec![]), // has a cursor already; nothing new
                    "Trash" => Ok((1..=10)
                        .filter(|uid| after_uid.is_none_or(|a| *uid > a))
                        .map(msg)
                        .collect()),
                    other => panic!("unexpected mailbox {other}"),
                }
            }
            fn first_uid_since(&mut self, date: &str) -> Result<Option<i64>, ProviderError> {
                assert_eq!(
                    self.current.borrow().as_str(),
                    "Trash",
                    "the window must only ever be consulted for Trash's fresh first import — \
                     INBOX already has a cursor and must resume from it, not re-search"
                );
                assert_eq!(date, "1-Jun-2026");
                Ok(Some(8)) // messages 8..=10 fall inside the window
            }
            fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
                match self.current.borrow().as_str() {
                    "INBOX" => Ok(vec![]),
                    "Trash" => Ok((1..=10)
                        .map(|uid| UidFlags {
                            uid,
                            is_seen: false,
                            is_flagged: false,
                            keywords: None,
                        })
                        .collect()),
                    other => panic!("unexpected mailbox {other}"),
                }
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }

        let db = temp_db();
        db.upsert_account(&Account {
            id: "acc".into(),
            email: "e@x".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.upsert_mailbox(&Mailbox {
            id: "acc:INBOX".into(),
            account_id: "acc".into(),
            path: "INBOX".into(),
            name: "INBOX".into(),
            role: Some("inbox".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.set_sync_cursor(
            "acc",
            "acc:INBOX",
            &SyncCursor {
                uid_validity: Some(1),
                uid_next: Some(2),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
        db.upsert_mailbox(&Mailbox {
            id: "acc:Trash".into(),
            account_id: "acc".into(),
            path: "Trash".into(),
            name: "Trash".into(),
            role: Some("trash".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.set_sync_cursor(
            "acc",
            "acc:Trash",
            &SyncCursor {
                uid_validity: Some(5),
                uid_next: Some(11),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();

        // Untick Trash (a folder-selection edit, castellum-db's
        // apply_mailbox_selection): keeps the role row, purges its cursor.
        db.apply_mailbox_selection("acc", &["INBOX".to_string()], false)
            .unwrap();
        assert!(
            db.get_sync_cursor("acc", "acc:Trash").unwrap().is_none(),
            "sanity: unticking purges the cursor"
        );
        assert!(
            db.list_mailboxes("acc")
                .unwrap()
                .iter()
                .any(|m| m.id == "acc:Trash" && m.role.as_deref() == Some("trash")),
            "sanity: the role row survives"
        );

        // Re-tick Trash and sync with an initial-history window: no cursor
        // means this is a fresh first-ever import, so the window applies —
        // exactly like discovering a brand-new mailbox.
        let mut mock = RoleReselectMock {
            current: RefCell::new(String::new()),
        };
        let report = sync_with_provider_scoped(
            &db,
            "acc",
            "e@x",
            Some(&["INBOX".to_string(), "Trash".to_string()]),
            Some("1-Jun-2026"),
            &mut mock,
            "test-host",
        )
        .unwrap();

        let trash = report.mailboxes.iter().find(|m| m.path == "Trash").unwrap();
        assert_eq!(
            trash.fetched, 3,
            "only UIDs 8..=10 — the window bounded the fresh first import"
        );
        assert_eq!(trash.fetched_uid_min, Some(8));
        assert_eq!(trash.total, 3);
        assert_eq!(db.count_messages("acc", "acc:Trash").unwrap(), 3);
    }

    // ---- MM-3 review: which mail the arrival rules see ----

    fn add_mark_read_rule(db: &Database) {
        db.upsert_filter(castellum_db::SavedFilterUpsert {
            id: None,
            account_id: Some("acc"),
            name: "Every message",
            kind: castellum_models::SavedFilterKind::Rule,
            group: &castellum_models::FilterGroup {
                op: castellum_models::BoolOp::And,
                conditions: vec![castellum_models::FilterCondition {
                    field: castellum_models::FilterField::Subject,
                    operator: castellum_models::FilterOperator::Contains,
                    value: castellum_models::FilterValue::Text("Message".into()),
                }],
                groups: vec![],
            },
            sort_field: None,
            sort_desc: true,
            trigger: Some(castellum_models::RuleTrigger::OnSync),
            actions: &[castellum_models::RuleAction::MarkRead],
        })
        .unwrap();
    }

    fn pending_mark_reads(db: &Database) -> Vec<String> {
        db.pending_operations("acc")
            .unwrap()
            .into_iter()
            .filter(|o| o.kind == "mark_read")
            .map(|o| o.payload_json)
            .collect()
    }

    #[test]
    fn arrival_rules_skip_a_first_import_and_a_uidvalidity_reset_but_see_new_mail() {
        let db = temp_db();
        seed_account_mailbox(&db);
        add_mark_read_rule(&db);
        let mut p = MockProvider {
            state: MailboxState {
                uid_validity: 1,
                uid_next: 3,
                exists: 2,
            },
            messages: vec![msg(1), msg(2)],
        };
        sync_with_provider(&db, "acc", "test@local.test", None, &mut p, "h").unwrap();
        assert!(
            pending_mark_reads(&db).is_empty(),
            "a first import is the mailbox's history, not arrivals"
        );

        p.messages.push(msg(3));
        p.state = MailboxState {
            uid_validity: 1,
            uid_next: 4,
            exists: 3,
        };
        sync_with_provider(&db, "acc", "test@local.test", None, &mut p, "h").unwrap();
        let ops = pending_mark_reads(&db);
        assert_eq!(ops.len(), 1, "{ops:?}");
        assert!(ops[0].contains("acc:INBOX:3"));

        // The server resets its UID space: every message is re-imported as
        // a "new" row, and none of them is an arrival. (The reset also
        // cancels the op queued above, which named a purged UID.)
        p.messages.push(msg(4));
        p.state = MailboxState {
            uid_validity: 2,
            uid_next: 5,
            exists: 4,
        };
        sync_with_provider(&db, "acc", "test@local.test", None, &mut p, "h").unwrap();
        assert!(
            pending_mark_reads(&db).is_empty(),
            "no rule ran over the re-imported mailbox: {:?}",
            pending_mark_reads(&db)
        );
    }

    // --- folders gone from the server (bug hunt 2026-10-12, #3456) ----------

    /// A server whose folder list the test rewrites between passes. Every
    /// folder is a real mailbox with its own messages, so a renamed folder
    /// is genuinely imported again under its new path.
    struct FolderListMock {
        folders: Vec<(RemoteMailbox, Vec<RemoteMessage>)>,
        selected: String,
        /// `session_abandoned()` reports this from the start of the pass.
        abandoned: bool,
    }

    impl FolderListMock {
        fn new(paths: &[&str]) -> Self {
            let mut mock = FolderListMock {
                folders: Vec::new(),
                selected: String::new(),
                abandoned: false,
            };
            for path in paths {
                mock.add(path, vec![msg(1), msg(2)]);
            }
            mock
        }
        fn add(&mut self, path: &str, messages: Vec<RemoteMessage>) {
            let mailbox = if path.eq_ignore_ascii_case("INBOX") {
                rmb_special(path, path, Some("inbox"), Some("/"), true, true)
            } else {
                rmb(path, path, None, Some("/"), true, true)
            };
            self.folders.push((mailbox, messages));
        }
        fn drop_folder(&mut self, path: &str) -> Vec<RemoteMessage> {
            let at = self.folders.iter().position(|(m, _)| m.path == path);
            self.folders.remove(at.expect("folder exists")).1
        }
        fn current(&self) -> &Vec<RemoteMessage> {
            &self
                .folders
                .iter()
                .find(|(m, _)| m.path == self.selected)
                .expect("selected folder exists")
                .1
        }
    }

    impl MailProvider for FolderListMock {
        fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
            Ok(self.folders.iter().map(|(m, _)| m.clone()).collect())
        }
        fn select(&mut self, path: &str) -> Result<MailboxState, ProviderError> {
            self.selected = path.to_string();
            let messages = self.current();
            Ok(MailboxState {
                uid_validity: 1,
                uid_next: messages.iter().map(|m| m.uid).max().unwrap_or(0) + 1,
                exists: messages.len() as i64,
            })
        }
        fn fetch_new(
            &mut self,
            after_uid: Option<i64>,
        ) -> Result<Vec<RemoteMessage>, ProviderError> {
            Ok(self
                .current()
                .iter()
                .filter(|m| after_uid.is_none_or(|a| m.uid > a))
                .cloned()
                .collect())
        }
        fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
            Ok(self
                .current()
                .iter()
                .map(|m| UidFlags {
                    keywords: None,
                    uid: m.uid,
                    is_seen: m.is_seen,
                    is_flagged: m.is_flagged,
                })
                .collect())
        }
        fn session_abandoned(&self) -> bool {
            self.abandoned
        }
        fn logout(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    fn folder_sync(db: &Database, mock: &mut FolderListMock) -> SyncReport {
        sync_with_provider(db, "acc", "test@local.test", None, mock, "test-host").unwrap()
    }

    fn stored_paths(db: &Database) -> Vec<String> {
        let mut paths: Vec<String> = db
            .list_mailboxes("acc")
            .unwrap()
            .into_iter()
            .map(|m| m.path)
            .collect();
        paths.sort();
        paths
    }

    /// INBOX + Projects, synced once so both hold mail and a cursor.
    fn synced_inbox_and_projects(db: &Database) -> FolderListMock {
        let mut mock = FolderListMock::new(&["INBOX", "Projects"]);
        let first = folder_sync(db, &mut mock);
        assert!(first.removed_mailboxes.is_empty());
        db.set_account_write_enabled("acc", true).unwrap();
        assert_eq!(db.count_messages("acc", "acc:Projects").unwrap(), 2);
        assert!(db.get_sync_cursor("acc", "acc:Projects").unwrap().is_some());
        mock
    }

    #[test]
    fn a_folder_gone_from_two_consecutive_listings_is_removed_with_its_mail() {
        let db = temp_db();
        let mut mock = synced_inbox_and_projects(&db);
        assert_eq!(db.search_messages("acc", "Message", 50).unwrap().len(), 4);
        mock.drop_folder("Projects");

        // First listing without it: only noted. One answer is not enough to
        // delete local mail.
        let report = folder_sync(&db, &mut mock);
        assert!(report.removed_mailboxes.is_empty());
        assert_eq!(stored_paths(&db), vec!["INBOX", "Projects"]);
        assert_eq!(db.count_messages("acc", "acc:Projects").unwrap(), 2);

        // Second consecutive listing without it: gone, with everything.
        let report = folder_sync(&db, &mut mock);
        assert_eq!(report.removed_mailboxes, vec!["Projects".to_string()]);
        assert_eq!(stored_paths(&db), vec!["INBOX"]);
        assert_eq!(db.count_messages("acc", "acc:Projects").unwrap(), 0);
        assert!(db.get_message_detail("acc:Projects:1").unwrap().is_none());
        assert!(db.get_sync_cursor("acc", "acc:Projects").unwrap().is_none());
        let hits = db.search_messages("acc", "Message", 50).unwrap();
        assert_eq!(hits.len(), 2, "search no longer returns the folder's mail");
        assert!(hits.iter().all(|h| h.mailbox_id == "acc:INBOX"));
        // INBOX is untouched, and the next pass has nothing left to do.
        assert_eq!(db.count_messages("acc", "acc:INBOX").unwrap(), 2);
        assert!(folder_sync(&db, &mut mock).removed_mailboxes.is_empty());
    }

    #[test]
    fn a_folder_that_reappears_starts_the_two_listing_count_again() {
        let db = temp_db();
        let mut mock = synced_inbox_and_projects(&db);
        let messages = mock.drop_folder("Projects");
        folder_sync(&db, &mut mock); // absent once
        mock.add("Projects", messages);
        folder_sync(&db, &mut mock); // back: the count is forgotten
        mock.drop_folder("Projects");
        let report = folder_sync(&db, &mut mock); // absent once again
        assert!(report.removed_mailboxes.is_empty());
        assert_eq!(db.count_messages("acc", "acc:Projects").unwrap(), 2);
        let report = folder_sync(&db, &mut mock);
        assert_eq!(report.removed_mailboxes, vec!["Projects".to_string()]);
    }

    #[test]
    fn an_empty_listing_never_removes_a_folder() {
        let db = temp_db();
        let mut mock = synced_inbox_and_projects(&db);
        mock.folders.clear();
        for _ in 0..3 {
            let report = folder_sync(&db, &mut mock);
            assert!(report.removed_mailboxes.is_empty());
        }
        assert_eq!(stored_paths(&db), vec!["INBOX", "Projects"]);
        assert_eq!(db.count_messages("acc", "acc:INBOX").unwrap(), 2);
        assert_eq!(db.count_messages("acc", "acc:Projects").unwrap(), 2);
    }

    #[test]
    fn a_listing_without_inbox_never_removes_a_folder() {
        let db = temp_db();
        let mut mock = synced_inbox_and_projects(&db);
        mock.add("Archive", vec![msg(1)]);
        folder_sync(&db, &mut mock);
        // A truncated listing: INBOX and Projects are both missing.
        mock.drop_folder("INBOX");
        mock.drop_folder("Projects");
        for _ in 0..3 {
            let report = folder_sync(&db, &mut mock);
            assert!(report.removed_mailboxes.is_empty());
        }
        assert_eq!(stored_paths(&db), vec!["Archive", "INBOX", "Projects"]);
        assert_eq!(db.count_messages("acc", "acc:Projects").unwrap(), 2);
        assert_eq!(db.count_messages("acc", "acc:INBOX").unwrap(), 2);
    }

    #[test]
    fn an_abandoned_session_never_removes_a_folder_and_restarts_the_count() {
        let db = temp_db();
        let mut mock = synced_inbox_and_projects(&db);
        mock.drop_folder("Projects");
        folder_sync(&db, &mut mock); // absent once, trustworthy
        mock.abandoned = true;
        for _ in 0..2 {
            let report = folder_sync(&db, &mut mock);
            assert!(report.removed_mailboxes.is_empty());
        }
        assert_eq!(db.count_messages("acc", "acc:Projects").unwrap(), 2);
        // The abandoned pass broke the run of consecutive listings: the
        // next healthy pass only notes the absence again.
        mock.abandoned = false;
        assert!(folder_sync(&db, &mut mock).removed_mailboxes.is_empty());
        assert_eq!(db.count_messages("acc", "acc:Projects").unwrap(), 2);
        let report = folder_sync(&db, &mut mock);
        assert_eq!(report.removed_mailboxes, vec!["Projects".to_string()]);
    }

    #[test]
    fn a_failed_listing_never_removes_a_folder() {
        struct ListFails;
        impl MailProvider for ListFails {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Err(ProviderError::Network("LIST failed".into()))
            }
            fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
                unreachable!("no mailbox is listed")
            }
            fn fetch_new(
                &mut self,
                _after_uid: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                unreachable!("no mailbox is listed")
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }
        let db = temp_db();
        let mut mock = synced_inbox_and_projects(&db);
        mock.drop_folder("Projects");
        folder_sync(&db, &mut mock); // absent once
        for _ in 0..2 {
            assert!(
                sync_with_provider(&db, "acc", "test@local.test", None, &mut ListFails, "h")
                    .is_err()
            );
        }
        assert_eq!(stored_paths(&db), vec!["INBOX", "Projects"]);
        assert_eq!(db.count_messages("acc", "acc:Projects").unwrap(), 2);
    }

    #[test]
    fn a_pending_op_on_a_folder_gone_from_the_server_is_cancelled_with_that_reason() {
        let db = temp_db();
        let mut mock = synced_inbox_and_projects(&db);
        db.mark_read("acc:Projects:1", true).unwrap();
        db.mark_read("acc:INBOX:1", true).unwrap();
        mock.drop_folder("Projects");
        folder_sync(&db, &mut mock);
        assert_eq!(
            db.pending_operations("acc").unwrap().len(),
            2,
            "nothing is cancelled on the first listing"
        );
        let report = folder_sync(&db, &mut mock);
        assert_eq!(report.removed_mailboxes, vec!["Projects".to_string()]);
        assert_eq!(report.removed_mailbox_cancelled_ops, 1);
        let pending = db.pending_operations("acc").unwrap();
        assert_eq!(pending.len(), 1, "the INBOX op is still queued");
        assert!(pending[0].payload_json.contains("acc:INBOX:1"));
        // The recorded reason ("cancelled: folder 'Projects' no longer
        // exists on the server") is pinned by the castellum-db test
        // `remove_mailboxes_gone_from_server_deletes_the_row_and_cancels_its_ops`.
    }

    /// Review of #3456: the rows went but the cached attachment bytes
    /// stayed whenever the removing pass was an IDLE resync, whose report
    /// nobody read. Driven through `resync_on_live_connection`, the exact
    /// call the IDLE supervisor makes, with its INBOX-only filter.
    #[test]
    fn an_idle_resync_that_removes_a_folder_unlinks_its_cached_attachments() {
        use crate::attachments::{cache_dir, cached_file_path};
        let db = temp_db();
        let profile_dir = castellum_testutil::scratch_root().join(format!(
            "castellum-sync-orphans-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&profile_dir);
        std::fs::create_dir_all(cache_dir(&profile_dir)).unwrap();

        let mut mock = synced_inbox_and_projects(&db);
        // "own" is referenced only from the folder about to disappear;
        // "shared" also from a message in INBOX, which stays.
        for hash in ["own", "shared"] {
            std::fs::write(cached_file_path(&profile_dir, hash), b"bytes").unwrap();
        }
        db.upsert_attachment_cache_entry("acc:Projects:1", 0, "own", 5, 1)
            .unwrap();
        db.upsert_attachment_cache_entry("acc:Projects:1", 1, "shared", 5, 1)
            .unwrap();
        db.upsert_attachment_cache_entry("acc:INBOX:1", 0, "shared", 5, 1)
            .unwrap();
        mock.drop_folder("Projects");

        let mut provider = connect_guarded_with(
            |_cfg| Ok(mock),
            &ImapConfig {
                host: "h".into(),
                port: 1,
                username: "u".into(),
                password: "p".into(),
                security: castellum_providers::TransportSecurity::Plaintext,
                allow_invalid_certs: false,
            },
        )
        .unwrap();
        let inbox_only = ["INBOX".to_string()];
        let mut idle_resync = || {
            resync_on_live_connection(
                &profile_dir,
                &db,
                "acc",
                "test@local.test",
                Some(&inbox_only[..]),
                &mut provider,
                "h",
            )
            .unwrap()
        };

        // First listing without the folder: nothing removed, nothing unlinked.
        assert!(idle_resync().removed_mailboxes.is_empty());
        assert!(cached_file_path(&profile_dir, "own").exists());

        let report = idle_resync();
        assert_eq!(report.removed_mailboxes, vec!["Projects".to_string()]);
        assert_eq!(report.orphaned_attachment_hashes, vec!["own".to_string()]);
        assert!(
            !cached_file_path(&profile_dir, "own").exists(),
            "bytes only the removed folder referenced are unlinked"
        );
        assert!(
            cached_file_path(&profile_dir, "shared").exists(),
            "bytes a message in another folder still references are kept"
        );
        assert_eq!(db.total_attachment_cache_bytes().unwrap(), 5);
        let _ = std::fs::remove_dir_all(&profile_dir);
    }

    /// Archiving on an account with no archive folder creates a local
    /// "Archive" row the server only gains when the move replays. Its
    /// absence from LIST is expected while the op is queued: never removed.
    #[test]
    fn a_folder_awaited_by_a_queued_move_is_never_removed() {
        let db = temp_db();
        let mut mock = synced_inbox_and_projects(&db);
        db.archive_message("acc:INBOX:1").unwrap();
        let dest: String = stored_paths(&db)
            .into_iter()
            .find(|p| p != "INBOX" && p != "Projects")
            .expect("archiving created a local destination folder");
        mock.folders[0].1.retain(|m| m.uid != 1);
        for _ in 0..3 {
            assert!(folder_sync(&db, &mut mock).removed_mailboxes.is_empty());
        }
        assert!(stored_paths(&db).contains(&dest));
        // Once the op has replayed and the server still does not list the
        // folder, the ordinary two-listing rule applies.
        replay_pending_ops(&db);
        assert!(folder_sync(&db, &mut mock).removed_mailboxes.is_empty());
        assert_eq!(folder_sync(&db, &mut mock).removed_mailboxes, vec![dest]);
    }

    #[test]
    fn a_filtered_sync_still_removes_a_folder_gone_from_the_server() {
        let db = temp_db();
        let mut mock = synced_inbox_and_projects(&db);
        mock.add("Archive", vec![msg(1)]);
        folder_sync(&db, &mut mock);
        mock.drop_folder("Projects");
        let filter = vec!["INBOX".to_string()];
        let filtered = |mock: &mut FolderListMock| {
            sync_with_provider(&db, "acc", "test@local.test", Some(&filter), mock, "h").unwrap()
        };
        assert!(filtered(&mut mock).removed_mailboxes.is_empty());
        let report = filtered(&mut mock);
        assert_eq!(report.removed_mailboxes, vec!["Projects".to_string()]);
        // Archive is outside the filter but still listed: it is not synced
        // this pass, and it is NOT removed.
        assert_eq!(stored_paths(&db), vec!["Archive", "INBOX"]);
        assert_eq!(db.count_messages("acc", "acc:Archive").unwrap(), 1);
    }

    #[test]
    fn a_renamed_folder_ends_with_exactly_one_copy_of_each_message() {
        let db = temp_db();
        let mut mock = synced_inbox_and_projects(&db);
        let messages = mock.drop_folder("Projects");
        mock.add("Projects-2024", messages);
        folder_sync(&db, &mut mock);
        folder_sync(&db, &mut mock);
        assert_eq!(stored_paths(&db), vec!["INBOX", "Projects-2024"]);
        assert_eq!(db.count_messages("acc", "acc:Projects").unwrap(), 0);
        assert_eq!(db.count_messages("acc", "acc:Projects-2024").unwrap(), 2);
        let hits = db.search_messages("acc", "Message", 50).unwrap();
        assert_eq!(hits.len(), 4, "two in INBOX, two in the renamed folder");
    }

    /// The stored INBOX is never a removal candidate, whatever casing the
    /// server lists it in.
    #[test]
    fn the_stored_inbox_is_never_removed_when_the_server_lists_it_in_another_case() {
        let db = temp_db();
        let mut mock = synced_inbox_and_projects(&db);
        let messages = mock.drop_folder("INBOX");
        mock.add("Inbox", messages);
        for _ in 0..3 {
            assert!(folder_sync(&db, &mut mock).removed_mailboxes.is_empty());
        }
        assert_eq!(db.count_messages("acc", "acc:INBOX").unwrap(), 2);
    }

    /// INBOX plus one plain folder, each with its own messages.
    struct TwoFolderMock {
        selected: String,
        inbox: Vec<RemoteMessage>,
        projects: Vec<RemoteMessage>,
    }

    impl TwoFolderMock {
        fn current(&self) -> &Vec<RemoteMessage> {
            if self.selected == "INBOX" {
                &self.inbox
            } else {
                &self.projects
            }
        }
    }

    impl MailProvider for TwoFolderMock {
        fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
            Ok(vec![
                rmb_special("INBOX", "INBOX", Some("inbox"), Some("/"), true, true),
                rmb("Projects", "Projects", None, Some("/"), true, true),
            ])
        }
        fn select(&mut self, path: &str) -> Result<MailboxState, ProviderError> {
            self.selected = path.to_string();
            let messages = self.current();
            Ok(MailboxState {
                uid_validity: 1,
                uid_next: messages.iter().map(|m| m.uid).max().unwrap_or(0) + 1,
                exists: messages.len() as i64,
            })
        }
        fn fetch_new(
            &mut self,
            after_uid: Option<i64>,
        ) -> Result<Vec<RemoteMessage>, ProviderError> {
            Ok(self
                .current()
                .iter()
                .filter(|m| after_uid.is_none_or(|a| m.uid > a))
                .cloned()
                .collect())
        }
        fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
            Ok(self
                .current()
                .iter()
                .map(|m| UidFlags {
                    keywords: None,
                    uid: m.uid,
                    is_seen: m.is_seen,
                    is_flagged: m.is_flagged,
                })
                .collect())
        }
        fn logout(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    #[test]
    fn arrival_rules_see_only_the_inbox_never_where_moves_land() {
        // A message a rule (or the user) moved into Projects shows up there
        // as a new row on the next sync; handing it to the rules again is
        // how two Move rules ping-ponged. Only Inbox arrivals qualify.
        let db = temp_db();
        seed_account_mailbox(&db);
        add_mark_read_rule(&db);
        let folders = vec!["INBOX".to_string(), "Projects".to_string()];
        let mut p = TwoFolderMock {
            selected: String::new(),
            inbox: vec![msg(1)],
            projects: vec![msg(2)],
        };
        sync_with_provider(&db, "acc", "test@local.test", Some(&folders), &mut p, "h").unwrap();
        assert!(pending_mark_reads(&db).is_empty());

        p.projects.push(msg(10));
        p.inbox.push(msg(11));
        sync_with_provider(&db, "acc", "test@local.test", Some(&folders), &mut p, "h").unwrap();
        let ops = pending_mark_reads(&db);
        assert_eq!(ops.len(), 1, "{ops:?}");
        assert!(ops[0].contains("acc:INBOX:11"), "{ops:?}");
    }

    // ---- S31b: one pass per account at a time, whoever triggered it ----

    /// Root message and a reply in its thread. Both subjects contain
    /// "Message", so the `add_mark_read_rule` arrival rule matches the reply;
    /// the reply lands in a muted thread, so mute-on-arrival fires for it too.
    fn thread_msg(uid: i64) -> RemoteMessage {
        let (subject, id, reply_to) = if uid == 1 {
            ("Message topic", "root", "")
        } else {
            (
                "Re: Message topic",
                "reply",
                "In-Reply-To: <root@example.com>\r\nReferences: <root@example.com>\r\n",
            )
        };
        RemoteMessage {
            uid,
            raw: format!(
                "From: Alice <alice@example.com>\r\nTo: test@local.test\r\n\
                 Subject: {subject}\r\nMessage-ID: <{id}@example.com>\r\n{reply_to}\
                 Date: Tue, 1 Jul 2025 1{uid}:00:00 +0000\r\n\r\nBody {uid}.\r\n"
            )
            .into_bytes(),
            is_seen: false,
            is_flagged: false,
        }
    }

    fn thread_mock(with_reply: bool) -> MockProvider {
        let n = if with_reply { 2 } else { 1 };
        MockProvider {
            state: MailboxState {
                uid_validity: 1,
                uid_next: n + 1,
                exists: n,
            },
            messages: (1..=n).map(thread_msg).collect(),
        }
    }

    /// A DB where the root is synced and muted, and the arrival rule exists:
    /// the reply (uid 2) is the one message every later pass may find new.
    fn db_awaiting_the_reply() -> Database {
        let db = temp_db();
        sync_with_provider(
            &db,
            "acc",
            "test@local.test",
            None,
            &mut thread_mock(false),
            "h",
        )
        .unwrap();
        add_mark_read_rule(&db);
        db.mute_thread("acc:INBOX:1", true).unwrap();
        db.set_account_write_enabled("acc", true).unwrap();
        db
    }

    /// Wraps a provider and reports how many callers are inside `fetch_new`
    /// at once. The first caller lingers there so an overlapping trigger has
    /// every chance to walk in alongside it.
    struct OverlapProbe {
        inner: MockProvider,
        inside: Arc<std::sync::atomic::AtomicUsize>,
        max_inside: Arc<std::sync::atomic::AtomicUsize>,
        entered: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl MailProvider for OverlapProbe {
        fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
            self.inner.list_mailboxes()
        }
        fn select(&mut self, path: &str) -> Result<MailboxState, ProviderError> {
            self.inner.select(path)
        }
        fn fetch_new(
            &mut self,
            after_uid: Option<i64>,
        ) -> Result<Vec<RemoteMessage>, ProviderError> {
            let now = self.inside.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_inside.fetch_max(now, Ordering::SeqCst);
            if self.entered.fetch_add(1, Ordering::SeqCst) == 0 {
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
            let out = self.inner.fetch_new(after_uid);
            self.inside.fetch_sub(1, Ordering::SeqCst);
            out
        }
        fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
            self.inner.list_uid_flags()
        }
        fn logout(&mut self) -> Result<(), ProviderError> {
            self.inner.logout()
        }
    }

    fn ops_for_the_reply(db: &Database) -> Vec<String> {
        db.pending_operations("acc")
            .unwrap()
            .into_iter()
            .filter(|o| o.payload_json.contains("acc:INBOX:2"))
            .map(|o| format!("{}:{}", o.kind, o.payload_json))
            .collect()
    }

    #[test]
    fn two_overlapping_triggers_for_one_account_are_serialised_and_count_a_new_message_once() {
        // Baseline: the same arrival handled by ONE pass.
        let control = db_awaiting_the_reply();
        let alone = sync_with_provider(
            &control,
            "acc",
            "test@local.test",
            None,
            &mut thread_mock(true),
            "h",
        )
        .unwrap();
        assert_eq!(alone.mailboxes[0].fetched, 1);
        let expected_ops = ops_for_the_reply(&control);
        assert!(
            !expected_ops.is_empty(),
            "the arrival rule / mute-on-arrival must act on the reply"
        );

        // Two triggers (say an IDLE wake and a periodic pass) for the same
        // account, the second arriving while the first is mid-pass.
        let db = Arc::new(db_awaiting_the_reply());
        let inside = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_inside = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let entered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let spawn_pass = |db: Arc<Database>| {
            let mut probe = OverlapProbe {
                inner: thread_mock(true),
                inside: Arc::clone(&inside),
                max_inside: Arc::clone(&max_inside),
                entered: Arc::clone(&entered),
            };
            std::thread::spawn(move || {
                sync_with_provider(&db, "acc", "test@local.test", None, &mut probe, "h").unwrap()
            })
        };
        let first = spawn_pass(Arc::clone(&db));
        while entered.load(Ordering::SeqCst) == 0 {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let second = spawn_pass(Arc::clone(&db));
        let reports = [first.join().unwrap(), second.join().unwrap()];

        assert_eq!(
            max_inside.load(Ordering::SeqCst),
            1,
            "two passes for one account ran at the same time"
        );
        let fetched: usize = reports.iter().map(|r| r.mailboxes[0].fetched).sum();
        assert_eq!(fetched, 1, "the reply must be counted as new exactly once");
        assert_eq!(
            ops_for_the_reply(&db),
            expected_ops,
            "arrival rules and mute-on-arrival must run exactly once for the reply"
        );
    }

    fn guarded_mock(mock: MockProvider) -> PanicGuardedProvider<MockProvider> {
        connect_guarded_with(
            |_cfg| Ok(mock),
            &ImapConfig {
                host: "h".into(),
                port: 1,
                username: "u".into(),
                password: "p".into(),
                security: castellum_providers::TransportSecurity::Plaintext,
                allow_invalid_certs: false,
            },
        )
        .unwrap_or_else(|_| unreachable!("the mock connects"))
    }

    #[test]
    fn a_failing_post_pass_hook_records_neither_success_nor_failure() {
        // S31b review, finding d: on main the display-name upsert ran before
        // the outcome was recorded, so when it failed the account row showed
        // neither a success nor a failure. The merge had moved it after.
        let db = temp_db();
        let mut provider = guarded_mock(thread_mock(false));
        let result = record_and_run_sync(
            &db,
            "acc",
            "test@local.test",
            None,
            None,
            &mut provider,
            "h",
            || Err(CoreError::Sync("display name upsert failed".into())),
        );
        assert!(result.is_err(), "the hook's error is the pass's error");
        let state = db.get_account_sync_state("acc").unwrap();
        assert!(
            state.as_ref().is_none_or(|s| s.last_success_at.is_none()
                && s.consecutive_failures == 0
                && s.last_error_kind.is_none()),
            "nothing may be recorded when the hook fails: {state:?}"
        );

        // Control: the same pass with a hook that succeeds records success.
        let db = temp_db();
        let mut provider = guarded_mock(thread_mock(false));
        record_and_run_sync(
            &db,
            "acc",
            "test@local.test",
            None,
            None,
            &mut provider,
            "h",
            || Ok(()),
        )
        .unwrap();
        let state = db.get_account_sync_state("acc").unwrap().unwrap();
        assert!(state.last_success_at.is_some());
    }

    /// S31b review, finding d, through the real call site. The unit test
    /// above only proves `record_and_run_sync` honours its hook; this one
    /// proves `sync_imap_json_with` wires the configured-name upsert into
    /// it BEFORE the outcome is recorded. The upsert is made to fail by a
    /// trigger installed over a second connection; main recorded neither
    /// success nor failure in that case, so the account must read neither.
    #[test]
    fn a_failing_display_name_upsert_records_neither_success_nor_failure() {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-sync-upsert-fail-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        let db = Database::open(&path).expect("open db");
        // The row exists already (a re-sync), so the upsert is an UPDATE.
        db.upsert_account(&Account {
            id: "acc-name".into(),
            email: "t@local.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        let side = rusqlite::Connection::open(&path).expect("second connection");
        side.execute_batch(
            "CREATE TRIGGER fail_account_update
             BEFORE UPDATE OF display_name ON accounts
             WHEN NEW.display_name IS NOT OLD.display_name
             BEGIN SELECT RAISE(ABORT, 'injected upsert failure'); END;",
        )
        .unwrap();

        // The sync itself preserves the stored name; only the configured
        // upsert changes it, so only that statement trips the trigger.
        let cfg = || {
            let mut cfg = sync_cfg("acc-name");
            cfg.display_name = Some("Configured Name".into());
            cfg
        };
        let result = sync_imap_json_with(&db, cfg(), |_cfg| Ok(thread_mock(false)));
        assert!(result.is_err(), "the upsert failure is the pass's error");
        let state = db.get_account_sync_state("acc-name").unwrap();
        assert!(
            state.as_ref().is_none_or(|s| s.last_success_at.is_none()
                && s.consecutive_failures == 0
                && s.last_error_kind.is_none()),
            "neither success nor failure may be recorded: {state:?}"
        );

        // Control: without the trigger the same call records a success.
        side.execute_batch("DROP TRIGGER fail_account_update;")
            .unwrap();
        sync_imap_json_with(&db, cfg(), |_cfg| Ok(thread_mock(false))).unwrap();
        let state = db.get_account_sync_state("acc-name").unwrap().unwrap();
        assert!(state.last_success_at.is_some());
    }

    // -- S59: notification outbox written at the end of a sync pass ---------

    /// Descriptor-path provider with a settable UIDVALIDITY and mailbox
    /// role, so the same mock plays first sync, warm resync, a UIDVALIDITY
    /// reset and a non-inbox folder.
    struct NotifyMock {
        descriptors: Vec<RemoteMessageDescriptor>,
        uid_validity: i64,
        path: &'static str,
        role: Option<&'static str>,
    }

    impl NotifyMock {
        fn inbox(descriptors: Vec<RemoteMessageDescriptor>) -> Self {
            Self {
                descriptors,
                uid_validity: 1,
                path: "INBOX",
                role: Some("inbox"),
            }
        }
    }

    impl MailProvider for NotifyMock {
        fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
            Ok(vec![rmb(
                self.path,
                self.path,
                self.role,
                Some("/"),
                true,
                true,
            )])
        }
        fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
            Ok(MailboxState {
                uid_validity: self.uid_validity,
                uid_next: self.descriptors.iter().map(|d| d.uid).max().unwrap_or(0) + 1,
                exists: self.descriptors.len() as i64,
            })
        }
        fn fetch_new(
            &mut self,
            _after_uid: Option<i64>,
        ) -> Result<Vec<RemoteMessage>, ProviderError> {
            Err(ProviderError::Protocol("body fetch should not run".into()))
        }
        fn fetch_new_descriptors(
            &mut self,
            after_uid: Option<i64>,
        ) -> Result<Vec<RemoteMessageDescriptor>, ProviderError> {
            Ok(self
                .descriptors
                .iter()
                .filter(|d| after_uid.is_none_or(|uid| d.uid > uid))
                .cloned()
                .collect())
        }
        fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
            Ok(self
                .descriptors
                .iter()
                .map(|d| UidFlags {
                    keywords: d.keywords.clone(),
                    uid: d.uid,
                    is_seen: d.is_seen,
                    is_flagged: d.is_flagged,
                })
                .collect())
        }
        fn logout(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    /// A descriptor "sent a minute ago" (the announce window is 24 h from now).
    fn fresh_descriptor(uid: i64, subject: &str) -> RemoteMessageDescriptor {
        RemoteMessageDescriptor {
            sent_at: Some(now_epoch() - 60),
            ..remote_descriptor(uid, subject)
        }
    }

    /// Drains the outbox (the public reader; it also marks rows delivered,
    /// which is fine at the end of a test).
    fn drain_outbox(db: &Database) -> Vec<castellum_models::PendingNotification> {
        db.take_pending_notifications(1000, now_epoch()).unwrap()
    }

    fn run_notify_sync(db: &Database, mock: &mut NotifyMock) {
        sync_with_provider(db, "acc", "test@local.test", None, mock, "test-host").unwrap();
    }

    #[test]
    fn first_sync_enqueues_no_notifications() {
        let db = temp_db();
        let descriptors = (1..=10)
            .map(|u| fresh_descriptor(u, "hello"))
            .collect::<Vec<_>>();
        let mut mock = NotifyMock::inbox(descriptors);
        run_notify_sync(&db, &mut mock);
        assert!(drain_outbox(&db).is_empty());
    }

    // --- bug hunt 2026-10-12 (#3456): resumed imports and missing UIDNEXT ---

    /// A first import interrupted after some chunks leaves a cursor row with
    /// a checkpoint and no UIDNEXT. Resuming it is still history: nothing of
    /// it may be announced as new mail.
    #[test]
    fn a_resumed_interrupted_first_import_enqueues_no_notifications() {
        let db = temp_db();
        let mut mock = NotifyMock::inbox((1..=10).map(|u| fresh_descriptor(u, "hello")).collect());
        // The state an interrupted chunked first import leaves behind.
        seed_account_mailbox(&db);
        db.set_descriptor_checkpoint("acc", "acc:INBOX", 1, 1)
            .unwrap();
        let cursor = db.get_sync_cursor("acc", "acc:INBOX").unwrap().unwrap();
        assert_eq!(cursor.uid_validity, Some(1));
        assert_eq!(cursor.uid_next, None);

        let report =
            sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
                .unwrap();
        assert!(
            report.mailboxes[0].fetched > 0,
            "the resume pass does import the rest"
        );
        assert!(drain_outbox(&db).is_empty());

        // The import is complete now: later mail is announced as usual.
        mock.descriptors.push(fresh_descriptor(11, "new one"));
        run_notify_sync(&db, &mut mock);
        let got = drain_outbox(&db);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].subject.as_deref(), Some("new one"));
    }

    /// A server whose SELECT carries no UIDNEXT (the provider reports 0).
    /// Records the `after_uid` of every descriptor fetch.
    struct NoUidNextMock {
        descriptors: Vec<RemoteMessageDescriptor>,
        after_uid_args: Vec<Option<i64>>,
    }

    impl MailProvider for NoUidNextMock {
        fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
            Ok(vec![rmb(
                "INBOX",
                "INBOX",
                Some("inbox"),
                Some("/"),
                true,
                true,
            )])
        }
        fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
            Ok(MailboxState {
                uid_validity: 7,
                uid_next: 0,
                exists: self.descriptors.len() as i64,
            })
        }
        fn fetch_new(
            &mut self,
            _after_uid: Option<i64>,
        ) -> Result<Vec<RemoteMessage>, ProviderError> {
            Err(ProviderError::Protocol("body fetch should not run".into()))
        }
        fn fetch_new_descriptors(
            &mut self,
            after_uid: Option<i64>,
        ) -> Result<Vec<RemoteMessageDescriptor>, ProviderError> {
            self.after_uid_args.push(after_uid);
            // What the IMAP provider does with a negative start: clamp it
            // to UID 1 and keep everything above `after_uid`.
            Ok(self
                .descriptors
                .iter()
                .filter(|d| after_uid.is_none_or(|uid| d.uid > uid))
                .cloned()
                .collect())
        }
        fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
            Ok(self
                .descriptors
                .iter()
                .map(|d| UidFlags {
                    keywords: None,
                    uid: d.uid,
                    is_seen: d.is_seen,
                    is_flagged: d.is_flagged,
                })
                .collect())
        }
        fn logout(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    #[test]
    fn a_server_without_uidnext_imports_each_message_once() {
        let db = temp_db();
        let mut mock = NoUidNextMock {
            descriptors: (1..=3).map(|u| fresh_descriptor(u, "old")).collect(),
            after_uid_args: Vec::new(),
        };
        let sync = |mock: &mut NoUidNextMock| {
            sync_with_provider(&db, "acc", "test@local.test", None, mock, "test-host").unwrap()
        };

        // Pass 1: the first import.
        assert_eq!(sync(&mut mock).mailboxes[0].fetched, 3);
        assert!(drain_outbox(&db).is_empty(), "a first import is history");
        let cursor = db.get_sync_cursor("acc", "acc:INBOX").unwrap().unwrap();
        assert_eq!(
            cursor.uid_next,
            Some(4),
            "never the server's missing value (0): one past the highest UID held"
        );

        // Pass 2, nothing new: nothing is fetched again, nothing is announced.
        assert_eq!(sync(&mut mock).mailboxes[0].fetched, 0);
        assert_eq!(mock.after_uid_args.last(), Some(&Some(3)));
        assert!(drain_outbox(&db).is_empty());

        // Pass 3, one new message: exactly that one arrives.
        mock.descriptors.push(fresh_descriptor(4, "new one"));
        assert_eq!(sync(&mut mock).mailboxes[0].fetched, 1);
        let got = drain_outbox(&db);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].subject.as_deref(), Some("new one"));
        assert_eq!(db.count_messages("acc", "acc:INBOX").unwrap(), 4);
    }

    /// A cursor an older build stored as UIDNEXT 0 is repaired: the pass
    /// resumes after the highest stored UID and announces nothing old.
    #[test]
    fn a_cursor_stored_with_uidnext_zero_does_not_reimport_the_mailbox() {
        let db = temp_db();
        seed_account_mailbox(&db);
        for uid in 1..=3 {
            db.upsert_message(&descriptor_message(uid)).unwrap();
        }
        seed_inbox_cursor(&db, 7, 0);
        let mut mock = NoUidNextMock {
            descriptors: (1..=3).map(|u| fresh_descriptor(u, "old")).collect(),
            after_uid_args: Vec::new(),
        };
        let report =
            sync_with_provider(&db, "acc", "test@local.test", None, &mut mock, "test-host")
                .unwrap();
        assert_eq!(report.mailboxes[0].fetched, 0);
        assert_eq!(mock.after_uid_args, vec![Some(3)]);
        assert!(drain_outbox(&db).is_empty());
        let cursor = db.get_sync_cursor("acc", "acc:INBOX").unwrap().unwrap();
        assert_eq!(cursor.uid_next, Some(4));
    }

    #[test]
    fn warm_resync_enqueues_new_unread_inbox_mail() {
        let db = temp_db();
        let mut mock = NotifyMock::inbox(vec![fresh_descriptor(1, "old")]);
        run_notify_sync(&db, &mut mock);
        mock.descriptors.push(fresh_descriptor(2, "new one"));
        mock.descriptors.push(fresh_descriptor(3, "new two"));
        run_notify_sync(&db, &mut mock);
        let got = drain_outbox(&db);
        let mut subjects: Vec<_> = got.iter().filter_map(|n| n.subject.clone()).collect();
        subjects.sort();
        assert_eq!(subjects, vec!["new one".to_string(), "new two".to_string()]);
    }

    #[test]
    fn uidvalidity_reset_enqueues_nothing() {
        let db = temp_db();
        let mut mock = NotifyMock::inbox(vec![fresh_descriptor(1, "old")]);
        run_notify_sync(&db, &mut mock);
        mock.uid_validity = 2;
        mock.descriptors = (1..=3).map(|u| fresh_descriptor(u, "again")).collect();
        run_notify_sync(&db, &mut mock);
        assert!(drain_outbox(&db).is_empty());
    }

    #[test]
    fn rule_mark_read_suppresses_notification() {
        let db = temp_db();
        let mut mock = NotifyMock::inbox(vec![fresh_descriptor(1, "old")]);
        run_notify_sync(&db, &mut mock);
        db.set_account_write_enabled("acc", true).unwrap();
        let group = castellum_models::FilterGroup {
            op: castellum_models::BoolOp::And,
            conditions: vec![castellum_models::FilterCondition {
                field: castellum_models::FilterField::Subject,
                operator: castellum_models::FilterOperator::Contains,
                value: castellum_models::FilterValue::Text("invoice".into()),
            }],
            groups: vec![],
        };
        db.upsert_filter(castellum_db::SavedFilterUpsert {
            id: None,
            account_id: Some("acc"),
            name: "Invoices",
            kind: castellum_models::SavedFilterKind::Rule,
            group: &group,
            sort_field: None,
            sort_desc: true,
            trigger: Some(castellum_models::RuleTrigger::OnSync),
            actions: &[castellum_models::RuleAction::MarkRead],
        })
        .unwrap();
        mock.descriptors
            .push(fresh_descriptor(2, "your invoice is ready"));
        run_notify_sync(&db, &mut mock);
        assert!(
            drain_outbox(&db).is_empty(),
            "a rule-read message is not announced"
        );
    }

    #[test]
    fn muted_thread_arrival_is_not_announced() {
        let db = temp_db();
        let mut mock = NotifyMock::inbox(vec![fresh_descriptor(1, "thread")]);
        run_notify_sync(&db, &mut mock);
        db.mute_thread("acc:INBOX:1", true).unwrap();
        let mut reply = fresh_descriptor(2, "Re: thread");
        reply.references = vec!["<d1@example.com>".into()];
        mock.descriptors.push(reply);
        run_notify_sync(&db, &mut mock);
        assert!(drain_outbox(&db).is_empty());
    }

    #[test]
    fn non_inbox_mailbox_never_enqueues() {
        let db = temp_db();
        let mut mock = NotifyMock {
            descriptors: vec![fresh_descriptor(1, "old")],
            uid_validity: 1,
            path: "Archive",
            role: Some("archive"),
        };
        run_notify_sync(&db, &mut mock);
        mock.descriptors.push(fresh_descriptor(2, "new in archive"));
        run_notify_sync(&db, &mut mock);
        assert!(drain_outbox(&db).is_empty());
    }

    #[test]
    fn overlapping_idle_and_periodic_passes_announce_a_new_message_once() {
        // Two triggers (an IDLE wake and the periodic timer) race for the same
        // account while one new message waits on the server. S31b serialises
        // the passes; the second must find nothing new and the outbox must hold
        // exactly one row for the message.
        let db = Arc::new(temp_db());
        let mut mock = NotifyMock::inbox(vec![fresh_descriptor(1, "old")]);
        run_notify_sync(&db, &mut mock);
        let descriptors = vec![fresh_descriptor(1, "old"), fresh_descriptor(2, "arrived")];
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let db = Arc::clone(&db);
                let mut m = NotifyMock::inbox(descriptors.clone());
                std::thread::spawn(move || run_notify_sync(&db, &mut m))
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        // A manual or launch pass afterwards must not announce it again, even
        // once the first banner was drained and marked delivered.
        let first = drain_outbox(&db);
        assert_eq!(first.len(), 1, "one notice per new message: {first:?}");
        assert_eq!(first[0].subject.as_deref(), Some("arrived"));
        let mut again = NotifyMock::inbox(descriptors);
        run_notify_sync(&db, &mut again);
        assert!(drain_outbox(&db).is_empty(), "re-announced by a later pass");
    }

    #[test]
    fn arrival_rule_move_out_of_the_inbox_suppresses_notification() {
        let db = temp_db();
        let mut mock = NotifyMock::inbox(vec![fresh_descriptor(1, "old")]);
        run_notify_sync(&db, &mut mock);
        db.set_account_write_enabled("acc", true).unwrap();
        // The rule's destination folder (the provider only lists the inbox,
        // so seed it directly, as a prior folder discovery would have).
        db.upsert_mailbox(&Mailbox {
            id: "acc:Receipts".into(),
            account_id: "acc".into(),
            path: "Receipts".into(),
            name: "Receipts".into(),
            role: None,
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        let group = castellum_models::FilterGroup {
            op: castellum_models::BoolOp::And,
            conditions: vec![castellum_models::FilterCondition {
                field: castellum_models::FilterField::Subject,
                operator: castellum_models::FilterOperator::Contains,
                value: castellum_models::FilterValue::Text("receipt".into()),
            }],
            groups: vec![],
        };
        db.upsert_filter(castellum_db::SavedFilterUpsert {
            id: None,
            account_id: Some("acc"),
            name: "Receipts",
            kind: castellum_models::SavedFilterKind::Rule,
            group: &group,
            sort_field: None,
            sort_desc: true,
            trigger: Some(castellum_models::RuleTrigger::OnSync),
            actions: &[castellum_models::RuleAction::Move {
                mailbox_id: "acc:Receipts".into(),
            }],
        })
        .unwrap();
        mock.descriptors
            .push(fresh_descriptor(2, "your receipt is attached"));
        mock.descriptors.push(fresh_descriptor(3, "lunch?"));
        run_notify_sync(&db, &mut mock);
        let got = drain_outbox(&db);
        let subjects: Vec<_> = got.iter().filter_map(|n| n.subject.clone()).collect();
        assert_eq!(
            subjects,
            vec!["lunch?".to_string()],
            "only the mail that stayed in the inbox is announced"
        );
    }
}
