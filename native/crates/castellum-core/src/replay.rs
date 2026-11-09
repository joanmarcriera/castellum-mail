// Castellum - write-path operation replay (executor).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Replays queued operations against the provider, oldest first. Idempotent:
// mark_read → IMAP `UID STORE \Seen`, so a retry (or replaying an already-applied
// op) is harmless. Each op walks queued → running → (succeeded | failed |
// failed_permanent | cancelled); `failed` retries with backoff, `failed_permanent`
// is the terminal "needs user" state once the attempt cap is hit. Blocking (no
// Tokio yet; the DB's single writer preserves ordering).

use crate::imap_connect::connect_guarded_imap;
use crate::redact;
use crate::CoreError;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use castellum_db::Database;
use castellum_models::OperationRecord;
use castellum_providers::{
    AppendedUid, Guarded, ImapConfig, KeywordStoreOutcome, MailProvider, MessageFlag,
    MessageIdMatches, ProviderError,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

/// Give up on an operation after this many attempts. At the cap it becomes the
/// terminal [`STATUS_FAILED_PERMANENT`], visible in the outbox for the user to
/// see/retry manually. Shared with the SMTP send executor (`crate::send`), which
/// drains the same queue with the same policy.
pub(crate) const MAX_ATTEMPTS: i64 = 8;

/// Write-path status strings. An op walks `queued` → `running` →
/// (`succeeded` | `failed` | `failed_permanent` | `cancelled`). `failed` is
/// retry-able (backoff); `failed_permanent` is terminal ("needs user").
pub(crate) const STATUS_FAILED: &str = "failed";
pub(crate) const STATUS_FAILED_PERMANENT: &str = "failed_permanent";

/// The status to record after an attempt fails: `failed` while retries remain,
/// or the terminal `failed_permanent` once the attempt cap is reached. The
/// executor never retries `failed_permanent`, so the outbox can surface it as a
/// distinct "needs your attention" state instead of a transient retry.
/// `attempts_after` is the attempt count AFTER the `running` transition's bump.
pub(crate) fn failure_status(attempts_after: i64) -> &'static str {
    if attempts_after >= MAX_ATTEMPTS {
        STATUS_FAILED_PERMANENT
    } else {
        STATUS_FAILED
    }
}

/// Backoff before retrying a `failed` op, in seconds: 0 after the first failure
/// (retry on the next trigger), then quadratic growth, capped at an hour.
pub(crate) fn backoff_seconds(attempts: i64) -> i64 {
    let a = (attempts - 1).max(0);
    a.saturating_mul(a).min(3600)
}

/// Whether an undelivered op should be attempted now. `queued`/`running` ops are
/// always due (until the attempt cap); a `failed` op is due once its backoff
/// window has elapsed and it has attempts left. Pure so the policy is unit-tested
/// directly, without a clock or a live queue.
///
/// `not_before_epoch` (undo-send window, migration 0020) is an additional gate
/// on top of the status machine above: a `queued`/`running`/`failed` op that
/// would otherwise be due still is NOT due while `now_epoch` hasn't reached it.
/// It is `None` for every op this doesn't apply to (everything except a
/// delayed `send`), so this is a no-op for the rest of the queue. `held` (set
/// only by the crash-safety startup reconciliation, never here) falls through
/// to the catch-all `_ => false`: it is never picked up by the normal executor,
/// only by an explicit `Database::send_now`/`Database::undo_send`.
pub(crate) fn due_for_attempt(op: &OperationRecord, now_epoch: i64) -> bool {
    if let Some(not_before) = op.not_before_epoch {
        if now_epoch < not_before {
            return false;
        }
    }
    match op.status.as_str() {
        "queued" => true,
        "running" => op.attempts < MAX_ATTEMPTS,
        "failed" => {
            op.attempts < MAX_ATTEMPTS
                && now_epoch >= op.updated_at_epoch + backoff_seconds(op.attempts)
        }
        // `cancelled` (UIDVALIDITY reset invalidated the target) is terminal.
        // Neither it nor `succeeded` is returned by `pending_operations`; this
        // arm is a second wall in case a caller hands one in directly.
        "cancelled" => false,
        // `failed_permanent` exhausted its attempts: terminal, never retried.
        // It IS still returned by `pending_operations` so the outbox can show it.
        "failed_permanent" => false,
        _ => false, // succeeded (never returned as pending), `held`, or unknown
    }
}

pub(crate) fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Retire operations that have exhausted their attempts to the terminal
/// [`STATUS_FAILED_PERMANENT`]. Covers ops left `running` by a crash on the final
/// attempt AND `failed` ops already at the cap — including any that exhausted
/// before `failed_permanent` existed, so an upgraded DB self-heals. Both are
/// already un-retryable via [`due_for_attempt`]; this only relabels them so the
/// outbox shows the honest terminal state. The existing `last_error` (the real
/// failure reason) is preserved.
///
/// `for_send` selects the owning executor: `true` = only `send` (SMTP) ops,
/// `false` = everything except `send`. Does not touch success/failure counters.
pub(crate) fn retire_exhausted_ops(
    db: &Database,
    pending: &[OperationRecord],
    for_send: bool,
) -> Result<(), CoreError> {
    for op in pending {
        if (op.kind == crate::send::SEND_KIND) != for_send {
            continue;
        }
        let exhausted =
            (op.status == "running" || op.status == STATUS_FAILED) && op.attempts >= MAX_ATTEMPTS;
        if exhausted {
            let reason = op
                .last_error
                .clone()
                .unwrap_or_else(|| "exhausted attempts".to_string());
            // Compare-and-swap on the snapshot, and never a `running` op this
            // launch is still executing on its last attempt:
            // retiring that would let Send Now / Retry requeue and re-claim it
            // mid-submission. A refusal is benign — the op is re-judged on the
            // next run, once its executor has recorded an outcome.
            db.retire_exhausted_operation(op.id, &op.status, MAX_ATTEMPTS, &reason)
                .map_err(db_err)?;
        }
    }
    Ok(())
}

/// Replay request (deserialized from the FFI JSON). Same connection shape as sync
/// — credentials are passed in per call (no persistence yet; Keychain is later).
///
/// `Debug` is implemented manually to redact `password` — a stray `{:?}` of a
/// config (e.g. in an error message) must never leak a credential.
#[derive(Deserialize)]
pub struct ReplayConfig {
    pub account_id: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    #[serde(default = "default_true")]
    pub use_tls: bool,
    /// IMAP security mode, "tls" | "starttls" | "none". Absent
    /// = the legacy `use_tls` decides; see `TransportSecurity::resolve`.
    #[serde(default)]
    pub imap_security: Option<String>,
    #[serde(default)]
    pub allow_invalid_certs: bool,
}

impl std::fmt::Debug for ReplayConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplayConfig")
            .field("account_id", &self.account_id)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("use_tls", &self.use_tls)
            .field("imap_security", &self.imap_security)
            .field("allow_invalid_certs", &self.allow_invalid_certs)
            .finish()
    }
}

fn default_true() -> bool {
    true
}

/// Outcome of a replay run.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ReplayReport {
    pub account_id: String,
    /// Operations applied successfully this run.
    pub applied: usize,
    /// Operations whose attempt failed in the execution loop this run — left
    /// `failed` (retryable) while attempts remain, or terminal `failed_permanent`
    /// at the cap. Does NOT count ops relabelled by `retire_exhausted_ops` (those
    /// exhausted on a previous run); query the outbox for `failed_permanent` to
    /// find everything needing attention.
    pub failed: usize,
    /// Undelivered mutating ops NOT attempted because the account is in
    /// evaluation (read-only) mode. They stay queued and visible in the outbox
    /// with no attempt consumed — nothing is sent to the server.
    #[serde(default)]
    pub blocked: usize,
}

/// Payload for a `mark_read` operation.
#[derive(Debug, Deserialize)]
struct MarkReadPayload {
    message_id: String,
    is_read: bool,
    /// Source coordinates stamped by `enqueue_move` when the message left
    /// before this op replayed (its local row is gone by then). Absent on
    /// ops for a message that never moved and on ops from older builds.
    #[serde(default)]
    src_path: Option<String>,
    #[serde(default)]
    uid: Option<i64>,
    /// The UIDVALIDITY `uid` belonged to when `enqueue_move` stamped it
    /// (0 or absent = not recorded; see `state_op_coords`).
    #[serde(default)]
    uid_validity: Option<i64>,
}

/// Payload for a `mark_all_read` operation. `uid_validity`/`uids` are the
/// snapshot `Database::mark_all_read` took of exactly the messages it flipped
/// (FP27): replay stores `\Seen` on those UIDs only, so mail that arrives
/// while the op waits is never marked read. Both are `None` only on an op
/// queued by an older build, which keeps its legacy whole-mailbox store.
#[derive(Debug, Deserialize)]
struct MarkAllReadPayload {
    mailbox_id: String,
    #[serde(default)]
    uid_validity: Option<i64>,
    #[serde(default)]
    uids: Option<String>,
}

/// Payload for a `flag` operation.
#[derive(Debug, Deserialize)]
struct FlagPayload {
    message_id: String,
    flagged: bool,
    /// Source coordinates stamped by `enqueue_move` when the message left
    /// before this op replayed (its local row is gone by then). Absent on
    /// ops for a message that never moved and on ops from older builds.
    #[serde(default)]
    src_path: Option<String>,
    #[serde(default)]
    uid: Option<i64>,
    /// The UIDVALIDITY `uid` belonged to when `enqueue_move` stamped it
    /// (0 or absent = not recorded; see `state_op_coords`).
    #[serde(default)]
    uid_validity: Option<i64>,
}

/// Payload for a `keyword` operation (S42: add/remove one IMAP keyword).
#[derive(Debug, Deserialize)]
struct KeywordPayload {
    message_id: String,
    keyword: String,
    add: bool,
    /// Source coordinates stamped by `enqueue_move` when the message left
    /// before this op replayed (its local row is gone by then). Absent on
    /// ops for a message that never moved and on ops from older builds.
    #[serde(default)]
    src_path: Option<String>,
    #[serde(default)]
    uid: Option<i64>,
    /// The UIDVALIDITY `uid` belonged to when `enqueue_move` stamped it
    /// (0 or absent = not recorded; see `state_op_coords`).
    #[serde(default)]
    uid_validity: Option<i64>,
}

/// Where a `mark_read` / `flag` / `keyword` op must land: the live local row
/// when the message is still here, else the snapshot `enqueue_move` stamped
/// into the payload just before it deleted that row. `None` only when neither
/// exists (an older payload, or mail that is genuinely gone) — the caller then
/// clears the op as moot.
///
/// The third element is the UIDVALIDITY that UID is known to belong to
///, which the store must find unchanged on the server:
/// - live row: the mailbox's sync cursor (a UIDVALIDITY reset seen by sync
///   purges the rows and cancels these ops, so a row that still exists was
///   fetched in the cursor's UID space);
/// - stamped coords: the value stamped with them, else (an op stamped by an
///   older build, or stamped before the mailbox had a cursor) the cursor.
///
/// `None` = no baseline is recorded anywhere; see [`guarded_write`].
fn state_op_coords(
    db: &Database,
    account_id: &str,
    message_id: &str,
    src_path: Option<String>,
    uid: Option<i64>,
    stamped_validity: Option<i64>,
) -> Result<Option<(String, i64, Option<i64>)>, CoreError> {
    if let Some((path, uid)) = db.message_imap_coords(message_id).map_err(db_err)? {
        let expected = db.mailbox_uid_validity(account_id, &path).map_err(db_err)?;
        return Ok(Some((path, uid, expected)));
    }
    let Some((path, uid)) = src_path.zip(uid) else {
        return Ok(None);
    };
    let expected = known_uid_validity(db, account_id, &path, stamped_validity)?;
    Ok(Some((path, uid, expected)))
}

/// The UIDVALIDITY a queued op's UID belongs to: the snapshot the op
/// carries when it has a usable (positive) one, else the mailbox's current
/// sync cursor. The fallback covers ops queued by a build that recorded no
/// snapshot and ops queued before the mailbox had a cursor (`0`). It is
/// sound because a reset that sync has already seen cancelled every such op
/// (`cancel_operations_for_purged_mailbox`); a reset sync has NOT seen yet
/// leaves the cursor at the old value, which then fails to match the server.
fn known_uid_validity(
    db: &Database,
    account_id: &str,
    path: &str,
    snapshot: Option<i64>,
) -> Result<Option<i64>, CoreError> {
    if let Some(v) = snapshot.filter(|v| *v > 0) {
        return Ok(Some(v));
    }
    db.mailbox_uid_validity(account_id, path).map_err(db_err)
}

/// Run one UIDVALIDITY-guarded provider write (`MailProvider::*_checked`)
/// and turn a refusal into the op's outcome. `action` reads
/// after "not"/"refusing to", e.g. "move a message".
///
/// - `expected` is `None` (nothing local records which UID space the UID is
///   from, e.g. the mailbox's first sync has not committed a cursor yet):
///   nothing is sent and the op fails RETRYABLY, so it applies once a sync
///   has recorded one.
/// - the server's UIDVALIDITY differs, or it reports none: the UID may name
///   a different message now. `NonRetryable`, straight to `failed_permanent`,
///   where the operations list shows the reason. The next sync purges and
///   re-imports the mailbox, so the message the user acted on comes back.
fn guarded_write<T>(
    path: &str,
    action: &str,
    expected: Option<i64>,
    write: impl FnOnce(i64) -> Result<Guarded<T>, ProviderError>,
) -> Result<T, CoreError> {
    let Some(expected) = expected else {
        return Err(CoreError::Sync(format!(
            "{path} has not finished its first sync, so its messages cannot be identified safely yet; will {action} once it has."
        )));
    };
    match write(expected).map_err(provider_err)? {
        Guarded::Applied(value) => Ok(value),
        Guarded::UidValidityChanged { current } => Err(CoreError::NonRetryable(format!(
            "{path} was rebuilt on the server since this change was queued (UIDVALIDITY {expected} -> {current}); refusing to {action} that may no longer be the same one."
        ))),
    }
}

/// Payload for a `move` operation (archive / delete-to-Trash). Self-contained:
/// the local row is gone by replay time, so coords travel in the op.
#[derive(Debug, Deserialize)]
struct MovePayload {
    src_path: String,
    uid: i64,
    message_id: String,
    dest_path: String,
    /// The source mailbox's UIDVALIDITY when the move was queued
    /// (`enqueue_move`). `0` = the mailbox had no sync cursor then; absent
    /// on an op from a build that did not read it back. See
    /// [`known_uid_validity`].
    #[serde(default)]
    uid_validity: Option<i64>,
}

/// Payload for an `undo_move` operation: a [`MovePayload`] with the roles
/// swapped (`src_path` = where the message sits now, `dest_path` = where it
/// is going back to), plus the id of the `move` it reverses.
#[derive(Debug, Deserialize)]
struct UndoMovePayload {
    src_path: String,
    message_id: String,
    dest_path: String,
    #[serde(default)]
    undo_of: Option<i64>,
}

/// Payload for an `empty_trash` operation. `uid_validity`/`uids` are the
/// snapshot `Database::empty_trash` took of exactly what the user saw as
/// Trash at action time — present on every op queued by this fix or later.
/// Both are `None` only for a LEGACY op enqueued before this fix (bare
/// `{"path": ...}`): `apply_operation` refuses those rather than ever
/// expunging by bare path again (see the doc on that match arm).
#[derive(Debug, Deserialize)]
struct EmptyTrashPayload {
    path: String,
    #[serde(default)]
    uid_validity: Option<i64>,
    #[serde(default)]
    uids: Option<String>,
}

/// Payload for a `delete_permanent` operation: `Database::empty_trash`
/// rewrites a still-pending `move`-to-Trash op into one of these, because
/// `enqueue_move` deletes the local row immediately but the server-side move
/// may not have happened yet — the message is not really in Trash to
/// snapshot (PR #165 review point 1). `uids` is always a single UID (the
/// move's source UID), kept as a string for the same
/// `MailProvider::expunge_mailbox` call `empty_trash` uses. Unlike
/// `EmptyTrashPayload`, every field is required: this op kind never existed
/// before the UID-snapshot fix, so there is no legacy shape to accept.
#[derive(Debug, Deserialize)]
struct DeletePermanentPayload {
    path: String,
    uid_validity: i64,
    uids: String,
    /// Set by `Database::delete_message_permanently` (a single Trash message
    /// the user deleted): the local row is kept until the server confirms,
    /// then removed here. Absent on ops rewritten from a pending move, whose
    /// local row was already dropped at enqueue time.
    #[serde(default)]
    message_id: Option<String>,
}

/// Shared by the `empty_trash` and `delete_permanent` arms: verify the
/// mailbox's CURRENT UIDVALIDITY still matches the snapshot taken at action
/// time, then expunge exactly `uids`. Fails closed (`NonRetryable`, no
/// provider call) on ANY non-positive UIDVALIDITY — snapshot or current —
/// not just on a plain mismatch: `0` is `Database::empty_trash`'s own
/// "no sync cursor yet" sentinel, and `ImapProvider::select` maps a
/// server that omits UIDVALIDITY entirely to `-1` (provider-imap lib.rs).
/// Treating either sentinel as "matches" would silently accept an unknown
/// UID space (PR #165 review point 2 — a bare `0 == 0` used to pass).
/// `uids` empty is the one case that always succeeds trivially: nothing to
/// expunge, so no SELECT/compare is needed.
fn expunge_snapshot(
    provider: &mut dyn MailProvider,
    path: &str,
    snapshot_validity: i64,
    uids: &str,
) -> Result<(), CoreError> {
    if uids.is_empty() {
        return Ok(());
    }
    let refused = |current: i64| {
        CoreError::NonRetryable(format!(
            "{path} UIDVALIDITY is unknown or changed since the snapshot was taken ({snapshot_validity} -> {current}); refusing to expunge stale UIDs."
        ))
    };
    // An unknown snapshot never reaches the provider at all.
    if snapshot_validity <= 0 {
        let current = provider.select(path).map_err(provider_err)?;
        return Err(refused(current.uid_validity));
    }
    // The compare happens inside the provider, on the SELECT the expunge
    // itself runs under: checking here and then calling the
    // unguarded `expunge_mailbox`, which SELECTs again, left a window in
    // which a rebuilt mailbox had the snapshot's UIDs expunged.
    //
    // `Unsupported` (PR #165 review point 3: no UIDPLUS) means retrying will
    // never help — the server's capabilities won't change between attempts —
    // so it is classified non-retryable here, distinct from every other
    // `ProviderError`, which stays a transient/retryable `Sync`.
    match provider.expunge_mailbox_checked(path, snapshot_validity, uids) {
        Ok(Guarded::Applied(())) => Ok(()),
        Ok(Guarded::UidValidityChanged { current }) => Err(refused(current)),
        Err(ProviderError::Unsupported(msg)) => Err(CoreError::NonRetryable(msg)),
        Err(other) => Err(provider_err(other)),
    }
}

/// Payload for a `sent_copy` operation (APPEND the delivered RFC822 bytes to
/// the Sent mailbox). The bytes live in `operation_blobs` keyed by the op id
/// (fetched at apply time, so listing pending ops never loads them); ops
/// queued by pre-blob builds may still carry them inline as `raw_b64`, which
/// remains an accepted fallback. Either way the copy survives crashes and
/// restarts and needs no draft/message row at replay time. `message_id` keys
/// the provider's duplicate guard. Carries NO UIDs, so a UIDVALIDITY purge
/// never cancels it (see `cancel_operations_for_purged_mailbox`).
#[derive(Debug, Deserialize)]
struct SentCopyPayload {
    dest_path: String,
    message_id: String,
    /// Legacy inline bytes from ops enqueued before `operation_blobs` existed.
    #[serde(default)]
    raw_b64: Option<String>,
}

/// Payload for an `import_eml` operation (APPEND an imported `.eml` file's
/// bytes to the destination mailbox). Same shape as `SentCopyPayload` by
/// design — both are "APPEND this blob, keyed by Message-ID for the
/// provider's duplicate guard" — kept as its own type (not shared) so each
/// operation kind stays self-documenting, matching FlagPayload/MovePayload/
/// EmptyTrashPayload/SentCopyPayload each having their own struct. No
/// `raw_b64` legacy fallback: `import_eml` never existed before
/// `operation_blobs`, so every row's bytes live there.
#[derive(Debug, Deserialize)]
struct ImportEmlPayload {
    dest_path: String,
    message_id: String,
}

/// Payload for a `server_draft_save` op. `message_id` is the RFC822
/// Message-ID already baked into the blob bytes (from `build_draft_rfc822`),
/// carried separately so the Message-ID search fallback below doesn't need
/// to re-parse the raw bytes.
#[derive(Debug, Deserialize)]
struct ServerDraftSavePayload {
    draft_id: i64,
    dest_path: String,
    message_id: String,
}

/// Payload for a `server_draft_cleanup` op. Self-contained (no draft row
/// needed at replay time) — same shape as `MovePayload`/`SentCopyPayload`.
///
/// `uid_validity` is the UID space `uid` was recorded in.
/// Absent on an op queued by an older build and `0` when it was never
/// known; replay deletes nothing for either.
#[derive(Debug, Deserialize)]
struct ServerDraftCleanupPayload {
    mailbox_path: String,
    uid: i64,
    #[serde(default)]
    uid_validity: Option<i64>,
}

pub(crate) fn provider_err(e: ProviderError) -> CoreError {
    CoreError::Sync(e.to_string())
}

pub(crate) fn db_err<E: std::fmt::Display>(e: E) -> CoreError {
    CoreError::Sync(format!("db: {e}"))
}

/// Parse the JSON config, connect an IMAP provider, replay, return a JSON report.
pub fn replay_imap_json(db: &Database, config_json: &str) -> Result<String, CoreError> {
    let cfg: ReplayConfig = serde_json::from_str(config_json)
        .map_err(|e| CoreError::Sync(format!("bad config json: {e}")))?;

    // Evaluation mode: do not even open a connection — return the blocked
    // report directly. (replay_with_provider re-checks as the authority.)
    if !db.account_write_enabled(&cfg.account_id).map_err(db_err)? {
        let pending = db.pending_operations(&cfg.account_id).map_err(db_err)?;
        let blocked = pending
            .iter()
            .filter(|op| op.kind != crate::send::SEND_KIND)
            .count();
        let report = ReplayReport {
            account_id: cfg.account_id.clone(),
            applied: 0,
            failed: 0,
            blocked,
        };
        return serde_json::to_string(&report)
            .map_err(|e| CoreError::Sync(format!("serialize report: {e}")));
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
    .inspect_err(|e| {
        // `redact_provider_error`'s summary is the same sanitized text the
        // account's diagnostic row would carry — safe to log, unlike `e`'s
        // own Display text.
        let (kind, summary) = redact::redact_provider_error(e);
        tracing::warn!(
            account_id = %cfg.account_id,
            kind = kind.as_str(),
            "replay: imap connect failed: {summary}"
        );
    })
    .map_err(provider_err)?;

    let report = replay_with_provider(db, &cfg.account_id, &mut provider)?;
    let _ = provider.logout();

    serde_json::to_string(&report).map_err(|e| CoreError::Sync(format!("serialize report: {e}")))
}

/// The thing a state op writes: kind + message (+ keyword for tags). `None`
/// for kinds whose ordering this guard does not manage.
fn state_op_target(op: &OperationRecord) -> Option<String> {
    if !matches!(op.kind.as_str(), "flag" | "mark_read" | "keyword") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&op.payload_json).ok()?;
    let message_id = v.get("message_id")?.as_str()?;
    let keyword = v
        .get("keyword")
        .and_then(|k| k.as_str())
        .unwrap_or("")
        .to_lowercase();
    Some(format!("{}\u{1f}{message_id}\u{1f}{keyword}", op.kind))
}

/// IMAP coordinates `(src_path, uid)` a state op was stamped with by
/// `enqueue_move` (it then replays against the departed message), if any.
fn stamped_coords(payload_json: &str) -> Option<(String, i64)> {
    let v: serde_json::Value = serde_json::from_str(payload_json).ok()?;
    Some((
        v.get("src_path")?.as_str()?.to_owned(),
        v.get("uid")?.as_i64()?,
    ))
}

fn release_owed(owed: &mut HashMap<(String, i64), usize>, op: &OperationRecord) {
    if let Some(n) = stamped_coords(&op.payload_json).and_then(|c| owed.get_mut(&c)) {
        *n = n.saturating_sub(1);
    }
}

/// Core replay algorithm, provider-agnostic (so tests can drive a mock).
///
/// Attempts each *due* undelivered op via the provider. A provider/coords failure
/// marks that op `failed` (recording the error) and continues — one bad op doesn't
/// block the rest. `failed` ops are retried on later runs with a backoff, up to
/// `MAX_ATTEMPTS`, after which they become the terminal `failed_permanent`
/// (visible in the outbox for manual action). Only a database error aborts the run.
pub fn replay_with_provider(
    db: &Database,
    account_id: &str,
    provider: &mut dyn MailProvider,
) -> Result<ReplayReport, CoreError> {
    let pending = db.pending_operations(account_id).map_err(db_err)?;

    // Evaluation mode (fail-closed): make NO changes to the mail server. Every
    // queued mutating op stays untouched and visible in the outbox; no attempt
    // is consumed and nothing is retired or applied. This is the enforcement
    // authority — reloadAccounts/replayNow/reSyncAll/outbox-retry/restart all
    // route here and cannot bypass it. `send` ops belong to the send executor.
    if !db.account_write_enabled(account_id).map_err(db_err)? {
        let blocked = pending
            .iter()
            .filter(|op| op.kind != crate::send::SEND_KIND)
            .count();
        return Ok(ReplayReport {
            account_id: account_id.to_string(),
            applied: 0,
            failed: 0,
            blocked,
        });
    }

    // Retire exhausted ops to the terminal `failed_permanent`: an op left `running`
    // by a crash on its final attempt, or a `failed` op already at the attempt cap
    // (including ops that exhausted BEFORE `failed_permanent` existed). Both are
    // already un-retryable via `due_for_attempt`; normalising them here makes the
    // outbox show the honest "needs your attention" state rather than a
    // transient-looking `failed`/`running`. The real failure reason is preserved.
    // `send` (SMTP) ops belong to the send executor — never touched here.
    retire_exhausted_ops(db, &pending, false)?;

    let now = now_epoch();
    let mut applied = 0usize;
    let mut failed = 0usize;
    let mut blocked = 0usize;

    // Targets (see `state_op_target`) with an older op still going to be
    // retried -- waiting out its backoff, kill-switched, or failed in THIS
    // pass. A newer op for a held target is skipped: it would land first and
    // the older one would replay later and flip the server back to a state
    // the user already reversed. Ops replay oldest first, so insertion order
    // here is id order.
    let mut held: HashSet<String> = HashSet::new();
    // Retryable state ops stamped with a departing message's coordinates ride
    // along with its move ("the flag travels with the move"). If one fails
    // this pass (or is not due), the move must wait: moved first, the retry
    // would find no message and the star/read change would be silently lost
    // (audit #3451). Counted per coordinate; a success or a terminal failure
    // releases its share. A dead `failed_permanent` op never blocks.
    let mut owed_before_move: HashMap<(String, i64), usize> = HashMap::new();
    for op in &pending {
        if matches!(op.kind.as_str(), "mark_read" | "flag" | "keyword")
            && matches!(op.status.as_str(), "queued" | "running" | "failed")
            && op.attempts < MAX_ATTEMPTS
        {
            if let Some(c) = stamped_coords(&op.payload_json) {
                *owed_before_move.entry(c).or_default() += 1;
            }
        }
    }
    for op in pending {
        // `send` (SMTP) ops are drained by `crate::send`, not this IMAP replayer.
        // Without this skip they'd hit apply_operation's unknown-kind arm and be
        // marked failed before the send executor ever saw them.
        if op.kind == crate::send::SEND_KIND {
            continue;
        }
        let target = state_op_target(&op);
        if target.as_ref().is_some_and(|t| held.contains(t)) {
            continue;
        }
        if op.kind == "move" {
            let waiting = stamped_coords(&op.payload_json)
                .is_some_and(|c| owed_before_move.get(&c).is_some_and(|n| *n > 0));
            if waiting {
                // Consume no attempt; the move goes once its state ops have.
                blocked += 1;
                continue;
            }
        }
        // Skip ops that aren't due yet (backoff) or have exhausted their attempts.
        if !due_for_attempt(&op, now) {
            if (op.status == "failed" || op.status == "running") && op.attempts < MAX_ATTEMPTS {
                held.extend(target);
            }
            continue;
        }
        // Live kill-switch: re-read the policy per op (cheap; replay is network-
        // bound) so disabling writes MID-BATCH stops the remainder. The entry
        // check alone would let an in-flight batch keep mutating the server after
        // the user flips to read-only. Leave the op queued; consume no attempt.
        if !db.account_write_enabled(account_id).map_err(db_err)? {
            blocked += 1;
            held.extend(target);
            continue;
        }
        // Claim the op (running + attempts bump) BEFORE applying. If the process
        // dies mid-op, the op stays `running` — still returned by
        // `pending_operations`, so the first run after relaunch retries it. Safe because every
        // op kind is idempotent (mark_read → STORE \Seen), or is made so on
        // a retry: a re-attempted `server_draft_save` looks for the copy an
        // earlier attempt may have appended before appending again (the tracker
        // #3389; `apply_operation` is told the prior attempt count). A refused claim
        // means the op went terminal (user cancel from the outbox, UIDVALIDITY
        // purge) after our pending snapshot — skip it so its server-side
        // effect never happens. It is a compare-and-swap on the snapshot's
        // status, and an op this launch is still running is never re-claimed
        //: a concurrent replay cannot apply it a second time.
        if !db
            .begin_operation_attempt_from(op.id, &op.status)
            .map_err(db_err)?
        {
            continue;
        }

        let result = apply_operation(
            db,
            provider,
            account_id,
            op.id,
            op.attempts,
            &op.kind,
            &op.payload_json,
        );
        match result {
            Ok(()) => {
                db.set_operation_status(op.id, "succeeded", None)
                    .map_err(db_err)?;
                applied += 1;
                release_owed(&mut owed_before_move, &op);
            }
            Err(e) => {
                // `attempts` was bumped by the `running` transition above, so the
                // effective count now is op.attempts + 1. `NonRetryable` skips
                // the normal attempt-count ladder entirely and goes straight to
                // the terminal state — a stale UIDVALIDITY snapshot or a legacy
                // payload will never succeed on a later attempt, so there is no
                // reason to let a destructive op sit `queued`/`failed` and get
                // retried against a UID space that no longer means what it did.
                let status = if matches!(e, CoreError::NonRetryable(_)) {
                    STATUS_FAILED_PERMANENT
                } else {
                    failure_status(op.attempts + 1)
                };
                // Log the classification only (op kind + redacted error
                // kind), not `e`'s Debug text — that goes to the DB row
                // below as it always has, but a log line follows the same
                // allowlist discipline as everywhere else in this module.
                let (kind, _) = redact::redact_core_error(&e);
                tracing::warn!(
                    account_id,
                    op_kind = %op.kind,
                    error_kind = kind.as_str(),
                    status,
                    "replay: operation failed"
                );
                // Display, not Debug: Debug wraps the message in the variant
                // name and quotes (`NonRetryable("...")`), which OutboxView
                // shows verbatim to the user (PR #165 review point 5). Every
                // variant's Display is already just its clean message.
                db.set_operation_status(op.id, status, Some(&format!("{e}")))
                    .map_err(db_err)?;
                failed += 1;
                if status == STATUS_FAILED_PERMANENT {
                    release_owed(&mut owed_before_move, &op);
                }
                if status != STATUS_FAILED_PERMANENT {
                    held.extend(target);
                }
            }
        }
    }

    Ok(ReplayReport {
        account_id: account_id.to_string(),
        applied,
        failed,
        blocked,
    })
}

/// Apply a single operation to the provider. Errors here are per-op (recorded as
/// `failed`); they don't abort the whole replay. `op_id` keys any large payload
/// bytes in `operation_blobs` (fetched only for the op being applied).
/// `prior_attempts` is how many times this op was claimed BEFORE this
/// attempt: non-zero means a retry, or the re-claim of an op an earlier
/// launch left `running`.
fn apply_operation(
    db: &Database,
    provider: &mut dyn MailProvider,
    account_id: &str,
    op_id: i64,
    prior_attempts: i64,
    kind: &str,
    payload_json: &str,
) -> Result<(), CoreError> {
    match kind {
        "mark_read" => {
            let p: MarkReadPayload = serde_json::from_str(payload_json)
                .map_err(|e| CoreError::Sync(format!("bad mark_read payload: {e}")))?;
            let coords = state_op_coords(
                db,
                account_id,
                &p.message_id,
                p.src_path,
                p.uid,
                p.uid_validity,
            )?;
            let Some((path, uid, expected)) = coords else {
                // No local row and no snapshot from a later move: the message
                // is gone, the read state is moot — succeed so the op doesn't
                // stick in the outbox forever.
                return Ok(());
            };
            guarded_write(&path, "change the read state of a message", expected, |v| {
                provider.store_seen_checked(&path, v, uid, p.is_read)
            })
        }
        "mark_all_read" => {
            let p: MarkAllReadPayload = serde_json::from_str(payload_json)
                .map_err(|e| CoreError::Sync(format!("bad mark_all_read payload: {e}")))?;
            let Some(path) = db.mailbox_path(&p.mailbox_id).map_err(db_err)? else {
                return Err(CoreError::Sync(format!(
                    "no mailbox path for {}",
                    p.mailbox_id
                )));
            };
            let (Some(snapshot_validity), Some(uids)) = (p.uid_validity, p.uids) else {
                // Legacy op (no snapshot): the old whole-mailbox store.
                return provider.mark_all_seen(&path).map_err(provider_err);
            };
            // Nothing the server knows about was flipped: no provider call.
            if uids.is_empty() {
                return Ok(());
            }
            // Same fail-closed UIDVALIDITY check as `expunge_snapshot`: a
            // changed or unknown UID space would mark the wrong mail read.
            // The provider compares on the SELECT the store runs under
            //, not on a separate one before it.
            let refused = |current: i64| {
                CoreError::NonRetryable(format!(
                    "{path} UIDVALIDITY is unknown or changed since Mark All as Read was pressed ({snapshot_validity} -> {current}); not marking stale UIDs read."
                ))
            };
            if snapshot_validity <= 0 {
                let current = provider.select(&path).map_err(provider_err)?;
                return Err(refused(current.uid_validity));
            }
            match provider
                .mark_seen_uids_checked(&path, snapshot_validity, &uids)
                .map_err(provider_err)?
            {
                Guarded::Applied(()) => Ok(()),
                Guarded::UidValidityChanged { current } => Err(refused(current)),
            }
        }
        "flag" => {
            let p: FlagPayload = serde_json::from_str(payload_json)
                .map_err(|e| CoreError::Sync(format!("bad flag payload: {e}")))?;
            let coords = state_op_coords(
                db,
                account_id,
                &p.message_id,
                p.src_path,
                p.uid,
                p.uid_validity,
            )?;
            let Some((path, uid, expected)) = coords else {
                // No local row and no move snapshot — flag state is moot;
                // succeed so the op clears instead of sticking failed.
                return Ok(());
            };
            guarded_write(&path, "flag a message", expected, |v| {
                provider.store_flagged_checked(&path, v, uid, p.flagged)
            })
        }
        "keyword" => {
            let p: KeywordPayload = serde_json::from_str(payload_json)
                .map_err(|e| CoreError::Sync(format!("bad keyword payload: {e}")))?;
            let coords = state_op_coords(
                db,
                account_id,
                &p.message_id,
                p.src_path.clone(),
                p.uid,
                p.uid_validity,
            )?;
            let Some((path, uid, expected)) = coords else {
                // No local row and no move snapshot — moot; clear it.
                return Ok(());
            };
            let outcome = guarded_write(&path, "tag a message", expected, |v| {
                provider.store_keyword_checked(&path, v, uid, &p.keyword, p.add)
            })?;
            match outcome {
                KeywordStoreOutcome::Stored => Ok(()),
                // The mailbox refuses this keyword permanently (no `\*` in
                // PERMANENTFLAGS, or it isn't listed): keep the tag on this
                // Mac only rather than failing the op forever (Assumption 5).
                // Only an ADD demotes — a refused removal has nothing to
                // demote (the local row is already gone). Demotion only
                // UPDATEs an existing row: if the user removed the tag
                // again before this replayed, nothing is resurrected.
                KeywordStoreOutcome::NotPermanent => {
                    if p.add {
                        let demoted = db
                            .demote_keyword_to_local(&p.message_id, &p.keyword)
                            .map_err(db_err)?;
                        // No local row: the message was moved away after
                        // the tag was set (`enqueue_move` deleted the row,
                        // and only stashed origin='local' tags). Hand the
                        // tag to the move op's stash so it re-attaches on
                        // arrival. No move op => the user removed the tag.
                        if !demoted {
                            if let (Some(src), Some(uid)) = (p.src_path.as_deref(), p.uid) {
                                db.stash_keyword_on_pending_move(
                                    account_id,
                                    op_id,
                                    &p.message_id,
                                    src,
                                    uid,
                                    &p.keyword,
                                )
                                .map_err(db_err)?;
                            }
                        }
                    }
                    Ok(())
                }
            }
        }
        "move" => {
            let p: MovePayload = serde_json::from_str(payload_json)
                .map_err(|e| CoreError::Sync(format!("bad move payload: {e}")))?;
            // The local row was deleted when the move was queued, so the
            // (path, uid) pair is all that names the message — and only
            // inside the UID space it was read from.
            let expected = known_uid_validity(db, account_id, &p.src_path, p.uid_validity)?;
            let landed = guarded_write(&p.src_path, "move a message", expected, |v| {
                provider.move_message_reporting(&p.src_path, v, p.uid, &p.message_id, &p.dest_path)
            })?;
            // Remember where it landed (COPYUID) so Undo can address it exactly.
            if let Some(m) = landed {
                db.record_move_result(op_id, m.uid_validity, m.uid)
                    .map_err(db_err)?;
            }
            Ok(())
        }
        "undo_move" => {
            let p: UndoMovePayload = serde_json::from_str(payload_json)
                .map_err(|e| CoreError::Sync(format!("bad undo_move payload: {e}")))?;
            // Did the move being reversed reach the server? Ops replay in id
            // order, so normally it has. If it is still waiting (backoff), the
            // message is still in `dest_path` and there is nothing to find yet:
            // retry. If it died for good the message never left, so the undo is
            // already satisfied (the local row stays gone until the folder is
            // next re-imported, exactly as after a manual Outbox cancel).
            if let Some(original) = p.undo_of {
                match db.operation_status(original).map_err(db_err)?.as_deref() {
                    Some("succeeded") | None => {}
                    Some("failed_permanent") | Some("cancelled") => return Ok(()),
                    Some(_) => {
                        return Err(CoreError::Sync(
                            "the change being undone has not reached the server yet".into(),
                        ))
                    }
                }
            }
            let couldnt = |why: &str| {
                CoreError::NonRetryable(format!("Couldn't undo: {why} Nothing was changed."))
            };
            // Preferred: the exact UID the server reported when the move ran
            // (COPYUID), guarded by the destination's UIDVALIDITY. Never a guess.
            let recorded = match p.undo_of {
                Some(original) => db
                    .operation_payload(original)
                    .map_err(db_err)?
                    .and_then(|j| serde_json::from_str::<serde_json::Value>(&j).ok())
                    .and_then(|v| {
                        Some((
                            v.get("dest_uid")?.as_i64()?,
                            v.get("dest_uid_validity")?.as_i64()?,
                        ))
                    }),
                None => None,
            };
            if let Some((uid, uid_validity)) = recorded.filter(|(u, v)| *u > 0 && *v > 0) {
                return guarded_write(
                    &p.src_path,
                    "move the message back",
                    Some(uid_validity),
                    |v| {
                        provider.move_message_checked(
                            &p.src_path,
                            v,
                            uid,
                            &p.message_id,
                            &p.dest_path,
                        )
                    },
                );
            }
            // Fallback: search by Message-ID. The provider returns only
            // candidates whose real header matches EXACTLY (SEARCH is a
            // substring match). Exactly one is required; zero or several fail
            // cleanly rather than moving a guess.
            if p.message_id.is_empty() {
                return Err(couldnt("the message has no Message-ID to find it by."));
            }
            let found = provider
                .locate_by_message_id(&p.src_path, &p.message_id)
                .map_err(provider_err)?;
            match found.as_slice() {
                [uid] => provider
                    .move_message(&p.src_path, *uid, &p.message_id, &p.dest_path)
                    .map_err(provider_err),
                [] => {
                    // Not where the move put it. If it is back in (or never left)
                    // the destination, the undo is already satisfied.
                    let back = provider
                        .locate_by_message_id(&p.dest_path, &p.message_id)
                        .map_err(provider_err)?;
                    if back.is_empty() {
                        Err(couldnt(&format!(
                            "the message was not found in {}.",
                            p.src_path
                        )))
                    } else {
                        Ok(())
                    }
                }
                _ => Err(couldnt(&format!(
                    "more than one message in {} has that Message-ID.",
                    p.src_path
                ))),
            }
        }
        "empty_trash" => {
            let p: EmptyTrashPayload = serde_json::from_str(payload_json)
                .map_err(|e| CoreError::Sync(format!("bad empty_trash payload: {e}")))?;
            // Legacy op from before the UID-snapshot fix: its payload is a
            // bare path with no UID set. Expunging "everything in Trash as
            // it stands now" is exactly the data-loss bug this fix closes —
            // refuse permanently rather than ever doing that again. The old
            // build already deleted the local Trash rows without recording
            // any snapshot, and incremental sync resumes from its stored
            // UIDNEXT/MODSEQ — it will NOT re-import what's already "seen" —
            // so simply telling the user to retry does nothing by itself.
            // Clear the mailbox's sync cursor so the next sync treats Trash
            // as never-synced and re-imports it (within the normal history
            // window) before the user retries.
            let (Some(snapshot_validity), Some(uids)) = (p.uid_validity, p.uids) else {
                db.clear_sync_cursor(account_id, &p.path).map_err(db_err)?;
                return Err(CoreError::NonRetryable(
                    "Empty Trash was queued by an older version. Trash will reload on the next sync; empty it again then.".into(),
                ));
            };
            expunge_snapshot(provider, &p.path, snapshot_validity, &uids)
        }
        "delete_permanent" => {
            let p: DeletePermanentPayload = serde_json::from_str(payload_json)
                .map_err(|e| CoreError::Sync(format!("bad delete_permanent payload: {e}")))?;
            expunge_snapshot(provider, &p.path, p.uid_validity, &p.uids)?;
            // Only a confirmed expunge removes the local row: on any error
            // above the message stays visible (and the op retries / lands in
            // the outbox as failed).
            if let Some(message_id) = &p.message_id {
                db.finish_permanent_delete(message_id).map_err(db_err)?;
            }
            Ok(())
        }
        "sent_copy" => {
            let p: SentCopyPayload = serde_json::from_str(payload_json)
                .map_err(|e| CoreError::Sync(format!("bad sent_copy payload: {e}")))?;
            let raw = match &p.raw_b64 {
                // Legacy op from a pre-blob build: bytes travel inline.
                Some(b64) => BASE64
                    .decode(b64)
                    .map_err(|e| CoreError::Sync(format!("bad sent_copy raw_b64: {e}")))?,
                None => db.operation_blob(op_id).map_err(db_err)?.ok_or_else(|| {
                    CoreError::Sync(format!("sent_copy op {op_id} has no stored bytes"))
                })?,
            };
            provider
                .append(&p.dest_path, &p.message_id, &raw)
                .map_err(provider_err)
        }
        "import_eml" => {
            let p: ImportEmlPayload = serde_json::from_str(payload_json)
                .map_err(|e| CoreError::Sync(format!("bad import_eml payload: {e}")))?;
            let raw = db.operation_blob(op_id).map_err(db_err)?.ok_or_else(|| {
                CoreError::Sync(format!("import_eml op {op_id} has no stored bytes"))
            })?;
            provider
                .append(&p.dest_path, &p.message_id, &raw)
                .map_err(provider_err)
        }
        "server_draft_save" => {
            let p: ServerDraftSavePayload = serde_json::from_str(payload_json)
                .map_err(|e| CoreError::Sync(format!("bad server_draft_save payload: {e}")))?;
            // S36 review fix: the 2s autosave debounce queues this op and
            // only then schedules replay, so a save can still be sitting in
            // the queue when the user presses Send or discards the draft.
            // If the draft is gone or no longer 'draft', skip the APPEND
            // entirely rather than mirroring a stale save to the server —
            // `record_server_draft_saved` (below) is the same guard for the
            // narrow race where status flips during the APPEND round-trip.
            //
            //: the same goes for a draft whose From account
            // changed since. This op belongs to `account_id` and its
            // `dest_path` is THAT account's Drafts; the draft now belongs to
            // another account, so appending would leave stale content in the
            // wrong account's Drafts.
            match db.draft_account_and_status(p.draft_id).map_err(db_err)? {
                Some((owner, status)) if status == "draft" && owner == account_id => {}
                _ => return Ok(()),
            }
            //: a RETRY may be running after an APPEND that
            // reached the server but whose answer (or whose record in the
            // DB) was lost. Appending again would leave the earlier copy in
            // Drafts for good — nothing tracks its UID. So an op that has
            // been attempted before first looks for its own Message-ID. A
            // first attempt never pays for that SELECT + SEARCH: it runs on
            // every 2s autosave.
            let already = if prior_attempts > 0 && !p.message_id.is_empty() {
                provider
                    .find_uids_by_message_id(&p.dest_path, &p.message_id)
                    .map_err(provider_err)?
            } else {
                MessageIdMatches {
                    uid_validity: 0,
                    uids: Vec::new(),
                }
            };
            let resolved = match already.uids.last().copied() {
                Some(newest) => {
                    // Keep the newest copy as the mirror and remove any
                    // other: several can exist when more than one attempt
                    // lost its answer.
                    for duplicate in &already.uids[..already.uids.len() - 1] {
                        db.queue_server_draft_cleanup(
                            account_id,
                            &p.dest_path,
                            already.uid_validity,
                            *duplicate,
                        )
                        .map_err(db_err)?;
                    }
                    Some(AppendedUid {
                        uid_validity: already.uid_validity,
                        uid: newest,
                    })
                }
                None => {
                    let raw = db.operation_blob(op_id).map_err(db_err)?.ok_or_else(|| {
                        CoreError::Sync(format!("server_draft_save op {op_id} has no stored bytes"))
                    })?;
                    let appended = provider
                        .append_with_flags_get_uid(&p.dest_path, &[MessageFlag::Draft], &raw)
                        .map_err(provider_err)?;
                    match appended {
                        Some(a) => Some(a),
                        None => provider
                            .find_uid_by_message_id(&p.dest_path, &p.message_id)
                            .map_err(provider_err)?,
                    }
                }
            };
            db.record_server_draft_saved(
                p.draft_id,
                account_id,
                &p.dest_path,
                resolved.map(|a| a.uid_validity),
                resolved.map(|a| a.uid),
                &p.message_id,
            )
            .map_err(db_err)?;
            Ok(())
        }
        "server_draft_cleanup" => {
            let p: ServerDraftCleanupPayload = serde_json::from_str(payload_json)
                .map_err(|e| CoreError::Sync(format!("bad server_draft_cleanup payload: {e}")))?;
            //: this is a permanent delete by bare UID, and a
            // UID means nothing outside the UIDVALIDITY it was recorded in.
            // When that is unknown (an op queued by an older build, or a
            // mirror recorded without one) or the mailbox now reports a
            // different value, the UID may name another message — a draft
            // written on the phone, say. Delete nothing and let the op
            // succeed: a leftover server draft is harmless and visible, a
            // wrongly expunged message is gone. Not surfaced as a failure,
            // because there is nothing the user could do with it.
            let expected = p.uid_validity.unwrap_or(0);
            if expected <= 0 {
                tracing::warn!(
                    account_id,
                    "replay: server draft cleanup has no recorded UIDVALIDITY; leaving the server copy in place"
                );
                return Ok(());
            }
            match provider
                .delete_uid_checked(&p.mailbox_path, expected, p.uid)
                .map_err(provider_err)?
            {
                Guarded::Applied(()) => Ok(()),
                Guarded::UidValidityChanged { current } => {
                    tracing::warn!(
                        account_id,
                        expected,
                        current,
                        "replay: Drafts UIDVALIDITY changed since the server draft was recorded; leaving the server copy in place"
                    );
                    Ok(())
                }
            }
        }
        other => Err(CoreError::Sync(format!("unknown operation kind: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use castellum_models::{Account, Address, Mailbox, NewDraft, NewMessage, SyncCursor};
    use castellum_providers::{AppendedUid, MailboxState, RemoteMailbox, RemoteMessage};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_db() -> Database {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-replay-test-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        Database::open(&path).expect("open db")
    }

    /// Records the store_seen calls it receives so tests can assert on them.
    #[derive(Default)]
    struct RecordingProvider {
        /// Every state store and move, in the order it reached the wire.
        order: Vec<&'static str>,
        calls: Vec<(String, i64, bool)>,
        mark_all_calls: Vec<String>,
        mark_seen_uids_calls: Vec<(String, String)>,
        flag_calls: Vec<(String, i64, bool)>,
        move_calls: Vec<(String, i64, String, String)>,
        expunge_calls: Vec<(String, String)>,
        append_get_uid_calls: Vec<(String, Vec<MessageFlag>, Vec<u8>)>,
        append_get_uid_returns: Option<AppendedUid>,
        find_uid_returns: Option<AppendedUid>,
        delete_uid_calls: Vec<(String, i64)>,
        keyword_calls: Vec<(String, i64, String, bool)>,
        /// What `store_keyword` returns when not failing (default `Stored`).
        keyword_outcome: Option<KeywordStoreOutcome>,
        fail: bool,
        /// Fail the next `store_flagged` only (a stale connection that works
        /// again after the reconnect), then behave normally.
        fail_flag_once: bool,
        /// When set, `expunge_mailbox` returns `Unsupported` instead of
        /// recording a call — simulates a server with no UIDPLUS (PR #165
        /// review point 3), which `expunge_snapshot` must classify
        /// `CoreError::NonRetryable`, distinct from the generic `fail` flag.
        fail_unsupported: bool,
        /// `select()`'s reported UIDVALIDITY. `None` (the default) reports
        /// `SEEDED_UID_VALIDITY`, the value the seed helpers record in the
        /// sync cursor, so an ordinary test sees a mailbox that was never
        /// reset; a test about the UIDVALIDITY check sets it explicitly.
        select_uid_validity: Option<i64>,
        /// Simulates a mailbox rebuilt AFTER a caller-visible `select` but
        /// before the write's own SELECT: the guarded bulk writes
        /// (`expunge_mailbox_checked`, `mark_seen_uids_checked`) refuse with
        /// this as the current UIDVALIDITY while `select()` still reports the
        /// old, matching one.
        rebuilt_before_write: Option<i64>,
        /// Test kill-switch: when set, the FIRST `store_seen` disables writes on
        /// this db (mid-batch), so the loop's per-op re-check must block the
        /// rest. A raw pointer keeps the mock lifetime-free; sound because the
        /// db outlives the provider within a single-threaded test.
        disable_writes_after_first: Option<*const Database>,
        /// What `locate_by_message_id` returns, per mailbox path.
        locate_returns: std::collections::HashMap<String, Vec<i64>>,
        /// COPYUID `move_message_reporting` reports (None = server sent none).
        reports_landing: Option<castellum_providers::MovedTo>,
    }

    impl MailProvider for RecordingProvider {
        fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
            Ok(vec![])
        }
        fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
            Ok(MailboxState {
                uid_validity: self.select_uid_validity.unwrap_or(SEEDED_UID_VALIDITY),
                uid_next: 1,
                exists: 0,
            })
        }
        fn fetch_new(&mut self, _after: Option<i64>) -> Result<Vec<RemoteMessage>, ProviderError> {
            Ok(vec![])
        }
        fn logout(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }
        fn store_seen(&mut self, path: &str, uid: i64, seen: bool) -> Result<(), ProviderError> {
            if self.fail {
                return Err(ProviderError::Protocol("boom".into()));
            }
            self.order.push("store_seen");
            self.calls.push((path.to_string(), uid, seen));
            if let Some(db) = self.disable_writes_after_first.take() {
                // SAFETY: test-only; the db outlives the provider and the test
                // is single-threaded.
                unsafe {
                    (*db).set_account_write_enabled("acc", false).unwrap();
                }
            }
            Ok(())
        }
        fn mark_all_seen(&mut self, path: &str) -> Result<(), ProviderError> {
            if self.fail {
                return Err(ProviderError::Protocol("boom".into()));
            }
            self.mark_all_calls.push(path.to_string());
            Ok(())
        }
        fn mark_seen_uids(&mut self, path: &str, uids: &str) -> Result<(), ProviderError> {
            if self.fail {
                return Err(ProviderError::Protocol("boom".into()));
            }
            self.mark_seen_uids_calls
                .push((path.to_string(), uids.to_string()));
            Ok(())
        }
        fn store_flagged(
            &mut self,
            path: &str,
            uid: i64,
            flagged: bool,
        ) -> Result<(), ProviderError> {
            if self.fail || std::mem::take(&mut self.fail_flag_once) {
                return Err(ProviderError::Protocol("boom".into()));
            }
            self.order.push("store_flagged");
            self.flag_calls.push((path.to_string(), uid, flagged));
            Ok(())
        }
        fn move_message(
            &mut self,
            src: &str,
            uid: i64,
            message_id: &str,
            dest: &str,
        ) -> Result<(), ProviderError> {
            if self.fail {
                return Err(ProviderError::Protocol("boom".into()));
            }
            self.order.push("move_message");
            self.move_calls.push((
                src.to_string(),
                uid,
                message_id.to_string(),
                dest.to_string(),
            ));
            Ok(())
        }
        fn move_message_reporting(
            &mut self,
            src: &str,
            expected: i64,
            uid: i64,
            message_id: &str,
            dest: &str,
        ) -> Result<Guarded<Option<castellum_providers::MovedTo>>, ProviderError> {
            let current = self.select(src)?.uid_validity;
            if !castellum_providers::uid_validity_matches(expected, current) {
                return Ok(Guarded::UidValidityChanged { current });
            }
            self.move_message(src, uid, message_id, dest)?;
            Ok(Guarded::Applied(self.reports_landing))
        }
        fn locate_by_message_id(
            &mut self,
            path: &str,
            _message_id: &str,
        ) -> Result<Vec<i64>, ProviderError> {
            Ok(self.locate_returns.get(path).cloned().unwrap_or_default())
        }
        fn expunge_mailbox_checked(
            &mut self,
            path: &str,
            expected: i64,
            uids: &str,
        ) -> Result<Guarded<()>, ProviderError> {
            if let Some(current) = self.rebuilt_before_write {
                return Ok(Guarded::UidValidityChanged { current });
            }
            let current = self.select(path)?.uid_validity;
            if !castellum_providers::uid_validity_matches(expected, current) {
                return Ok(Guarded::UidValidityChanged { current });
            }
            self.expunge_mailbox(path, uids).map(Guarded::Applied)
        }
        fn mark_seen_uids_checked(
            &mut self,
            path: &str,
            expected: i64,
            uids: &str,
        ) -> Result<Guarded<()>, ProviderError> {
            if let Some(current) = self.rebuilt_before_write {
                return Ok(Guarded::UidValidityChanged { current });
            }
            let current = self.select(path)?.uid_validity;
            if !castellum_providers::uid_validity_matches(expected, current) {
                return Ok(Guarded::UidValidityChanged { current });
            }
            self.mark_seen_uids(path, uids).map(Guarded::Applied)
        }
        fn expunge_mailbox(&mut self, path: &str, uids: &str) -> Result<(), ProviderError> {
            if self.fail_unsupported {
                return Err(ProviderError::Unsupported("no UIDPLUS".into()));
            }
            if self.fail {
                return Err(ProviderError::Protocol("boom".into()));
            }
            self.expunge_calls
                .push((path.to_string(), uids.to_string()));
            Ok(())
        }
        fn append_with_flags_get_uid(
            &mut self,
            mailbox_path: &str,
            flags: &[MessageFlag],
            raw: &[u8],
        ) -> Result<Option<AppendedUid>, ProviderError> {
            if self.fail {
                return Err(ProviderError::Protocol("boom".into()));
            }
            self.append_get_uid_calls.push((
                mailbox_path.to_string(),
                flags.to_vec(),
                raw.to_vec(),
            ));
            Ok(self.append_get_uid_returns)
        }
        fn find_uid_by_message_id(
            &mut self,
            _mailbox_path: &str,
            _message_id: &str,
        ) -> Result<Option<AppendedUid>, ProviderError> {
            Ok(self.find_uid_returns)
        }
        fn delete_uid(&mut self, mailbox_path: &str, uid: i64) -> Result<(), ProviderError> {
            if self.fail {
                return Err(ProviderError::Protocol("boom".into()));
            }
            self.delete_uid_calls.push((mailbox_path.to_string(), uid));
            Ok(())
        }
        fn store_keyword(
            &mut self,
            path: &str,
            uid: i64,
            keyword: &str,
            add: bool,
        ) -> Result<KeywordStoreOutcome, ProviderError> {
            if self.fail {
                return Err(ProviderError::Protocol("boom".into()));
            }
            self.order.push("store_keyword");
            self.keyword_calls
                .push((path.to_string(), uid, keyword.to_string(), add));
            Ok(self.keyword_outcome.unwrap_or(KeywordStoreOutcome::Stored))
        }
    }

    /// The UIDVALIDITY the seed helpers record for a mailbox and
    /// `RecordingProvider::select` reports unless a test overrides it.
    const SEEDED_UID_VALIDITY: i64 = 1;

    /// Record `uid_validity` as the mailbox's synced UID space, as a real
    /// sync does before any message of it can be acted on.
    fn seed_cursor(db: &Database, mailbox_id: &str, uid_validity: i64) {
        db.set_sync_cursor(
            "acc",
            mailbox_id,
            &SyncCursor {
                uid_validity: Some(uid_validity),
                uid_next: Some(100),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
    }

    fn seed_message(db: &Database) {
        db.upsert_account(&Account {
            id: "acc".into(),
            email: "e@x".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        // These tests exercise remote mutations, so leave evaluation mode.
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
        seed_cursor(db, "acc:INBOX", SEEDED_UID_VALIDITY);
        db.upsert_message(&NewMessage {
            id: "acc:INBOX:7".into(),
            account_id: "acc".into(),
            mailbox_id: "acc:INBOX".into(),
            provider_uid: Some(7),
            rfc822_message_id: Some("<7@x>".into()),
            subject: Some("Hi".into()),
            from_name: None,
            from_address: Some("a@x".into()),
            sent_at: Some(1000),
            snippet: None,
            size_bytes: Some(10),
            has_attachments: false,
            attachments: None,
            is_read: false,
            is_starred: false,
            addresses: vec![Address {
                kind: "from".into(),
                name: None,
                address: "a@x".into(),
            }],
            text_body: Some("body".into()),
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: vec![],
        })
        .unwrap();
    }

    fn empty_draft(account_id: &str) -> NewDraft {
        NewDraft {
            id: None,
            account_id: account_id.to_string(),
            to: vec![],
            cc: vec![],
            bcc: vec![],
            subject: "Hi".into(),
            body_text: "".into(),
            body_html: None,
            from_email: None,
            from_name: None,
            in_reply_to: None,
            references_header: None,
        }
    }

    #[test]
    fn server_draft_save_records_the_appended_uid_when_uidplus_present() {
        let db = temp_db();
        db.upsert_account(&Account {
            id: "acc".into(),
            email: "a@x".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.set_account_write_enabled("acc", true).unwrap();
        let draft_id = db.save_draft(&empty_draft("acc")).unwrap();
        db.queue_server_draft_save(draft_id, "<m1@x>", b"raw")
            .unwrap();

        let mut provider = RecordingProvider {
            append_get_uid_returns: Some(AppendedUid {
                uid_validity: 1,
                uid: 42,
            }),
            ..Default::default()
        };
        let report = replay_with_provider(&db, "acc", &mut provider).unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(provider.append_get_uid_calls.len(), 1);
        assert_eq!(provider.append_get_uid_calls[0].1, vec![MessageFlag::Draft]);
        let draft = db.get_draft(draft_id).unwrap().unwrap();
        assert_eq!(draft.server_uid, Some(42));
    }

    #[test]
    fn server_draft_save_falls_back_to_message_id_search_without_appenduid() {
        let db = temp_db();
        db.upsert_account(&Account {
            id: "acc".into(),
            email: "a@x".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.set_account_write_enabled("acc", true).unwrap();
        let draft_id = db.save_draft(&empty_draft("acc")).unwrap();
        db.queue_server_draft_save(draft_id, "<m1@x>", b"raw")
            .unwrap();

        let mut provider = RecordingProvider {
            append_get_uid_returns: None,
            find_uid_returns: Some(AppendedUid {
                uid_validity: 1,
                uid: 77,
            }),
            ..Default::default()
        };
        replay_with_provider(&db, "acc", &mut provider).unwrap();
        assert_eq!(
            db.get_draft(draft_id).unwrap().unwrap().server_uid,
            Some(77)
        );
    }

    #[test]
    fn server_draft_cleanup_calls_delete_uid() {
        let db = temp_db();
        db.upsert_account(&Account {
            id: "acc".into(),
            email: "a@x".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.set_account_write_enabled("acc", true).unwrap();
        let draft_id = db.save_draft(&empty_draft("acc")).unwrap();
        // First save: nothing to clean up. Second save (edit): supersedes the
        // first uid -> queues its cleanup as a self-contained op.
        db.record_server_draft_saved(draft_id, "acc", "Drafts", Some(1), Some(9), "<m1@x>")
            .unwrap();
        db.record_server_draft_saved(draft_id, "acc", "Drafts", Some(1), Some(10), "<m2@x>")
            .unwrap();

        let mut provider = RecordingProvider::default();
        let report = replay_with_provider(&db, "acc", &mut provider).unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(provider.delete_uid_calls, vec![("Drafts".to_string(), 9)]);
    }

    #[test]
    fn replay_applies_mark_read_and_clears_the_queue() {
        let db = temp_db();
        seed_message(&db);
        db.mark_read("acc:INBOX:7", true).unwrap();

        let mut provider = RecordingProvider {
            fail: false,
            ..Default::default()
        };
        let report = replay_with_provider(&db, "acc", &mut provider).unwrap();

        assert_eq!(report.applied, 1);
        assert_eq!(report.failed, 0);
        assert_eq!(provider.calls, vec![("INBOX".to_string(), 7, true)]);
        // Op is no longer pending (succeeded).
        assert!(db.pending_operations("acc").unwrap().is_empty());

        // Idempotent: replaying again with no new ops is a no-op.
        let again = replay_with_provider(&db, "acc", &mut provider).unwrap();
        assert_eq!(again.applied, 0);
    }

    #[test]
    fn evaluation_mode_blocks_replay_and_leaves_ops_queued() {
        let db = temp_db();
        seed_message(&db); // enables writes
        db.mark_read("acc:INBOX:7", true).unwrap(); // op queued while writable
        db.set_account_write_enabled("acc", false).unwrap(); // enter evaluation mode

        let mut provider = RecordingProvider {
            fail: false,
            ..Default::default()
        };
        let report = replay_with_provider(&db, "acc", &mut provider).unwrap();

        assert_eq!(report.applied, 0, "nothing is applied in evaluation mode");
        assert_eq!(report.blocked, 1, "the queued op is reported blocked");
        assert!(
            provider.calls.is_empty(),
            "no STORE ever reaches the server"
        );
        // The op stays queued and visible; no attempt is consumed (no spin).
        let pending = db.pending_operations("acc").unwrap();
        assert_eq!(pending.len(), 1, "op stays queued");
        assert_eq!(pending[0].attempts, 0, "no attempt consumed");

        // Re-enabling writes lets the queued op through unchanged.
        db.set_account_write_enabled("acc", true).unwrap();
        let report = replay_with_provider(&db, "acc", &mut provider).unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(report.blocked, 0);
        assert_eq!(provider.calls, vec![("INBOX".to_string(), 7, true)]);
    }

    #[test]
    fn disabling_writes_mid_batch_stops_the_remaining_ops() {
        // Live kill-switch: with a batch already in flight, flipping the account
        // to read-only mid-loop must stop the remaining ops (the entry check
        // alone would let the whole batch finish).
        let db = temp_db();
        seed_message(&db); // acc:INBOX:7, writes enabled
        seed_msg(&db, "acc:INBOX:8", "acc:INBOX", 8, "<8@x>");
        db.mark_read("acc:INBOX:7", true).unwrap();
        db.mark_read("acc:INBOX:8", true).unwrap();

        let mut provider = RecordingProvider {
            disable_writes_after_first: Some(&db as *const Database),
            ..Default::default()
        };
        let report = replay_with_provider(&db, "acc", &mut provider).unwrap();

        // Exactly one STORE reached the server before the switch; the rest blocked.
        assert_eq!(report.applied, 1);
        assert_eq!(report.blocked, 1);
        assert_eq!(provider.calls.len(), 1, "only one STORE reached the server");
        // The second op stays queued with no attempt consumed (retry when writable).
        let pending = db.pending_operations("acc").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].attempts, 0);
    }

    #[test]
    fn replay_never_applies_an_op_cancelled_after_the_snapshot() {
        let db = temp_db();
        seed_message(&db);
        db.mark_read("acc:INBOX:7", true).unwrap();
        let op_id = db.pending_operations("acc").unwrap()[0].id;

        // The user cancels from the outbox between the executor's pending
        // snapshot and its claim. The claim must refuse and the server-side
        // effect must never happen. (Cancelling before replay runs models the
        // same interleaving: the snapshot inside replay_with_provider is
        // taken from the same serialized connection.)
        assert!(db.cancel_operation(op_id).unwrap());

        let mut provider = RecordingProvider {
            fail: false,
            ..Default::default()
        };
        let report = replay_with_provider(&db, "acc", &mut provider).unwrap();

        assert_eq!(report.applied, 0);
        assert_eq!(report.failed, 0);
        assert!(
            provider.calls.is_empty(),
            "a cancelled op must not reach the provider"
        );
        assert!(db.pending_operations("acc").unwrap().is_empty());
    }

    #[test]
    fn manual_retry_makes_a_permanently_failed_op_replayable_again() {
        let db = temp_db();
        seed_message(&db);
        db.mark_read("acc:INBOX:7", true).unwrap();
        let op_id = db.pending_operations("acc").unwrap()[0].id;
        db.set_operation_status(op_id, "running", None).unwrap();
        db.set_operation_status(op_id, "failed_permanent", Some("gave up"))
            .unwrap();

        // Retired ops are invisible to the executor…
        let mut provider = RecordingProvider {
            fail: false,
            ..Default::default()
        };
        let before = replay_with_provider(&db, "acc", &mut provider).unwrap();
        assert_eq!(before.applied, 0);
        assert!(provider.calls.is_empty());

        // …until the user retries from the outbox; then one replay applies it.
        assert!(db.retry_operation(op_id).unwrap());
        let after = replay_with_provider(&db, "acc", &mut provider).unwrap();
        assert_eq!(after.applied, 1);
        assert_eq!(provider.calls, vec![("INBOX".to_string(), 7, true)]);
        assert!(db.pending_operations("acc").unwrap().is_empty());
    }

    /// Real IMAP round-trip against a local GreenMail (scripts/local-imap).
    /// Ignored by default; run with `cargo test -- --ignored`. Provisions its
    /// own account (a fresh unread bootstrap message arrives every run).
    #[test]
    #[ignore = "requires a local GreenMail (scripts/local-imap) on 3025/3143"]
    fn greenmail_replay_sets_seen_on_server() {
        use castellum_provider_imap::ImapProvider;
        let user = gm_provision("replay-seen");
        let cfg = gm_cfg(&user);
        let db = temp_db();

        // Sync INBOX so we have real messages + provider uids.
        let mut p = ImapProvider::connect(&cfg).unwrap();
        crate::sync::sync_with_provider(
            &db,
            "gm",
            &user,
            Some(&["INBOX".to_string()]),
            &mut p,
            "test-host",
        )
        .unwrap();
        let _ = p.logout();
        // This e2e exercises a real STORE, so leave evaluation mode.
        db.set_account_write_enabled("gm", true).unwrap();

        // Mark an unread message read locally + enqueue.
        let headers = db.list_message_headers("gm", "gm:INBOX", 200, 0).unwrap();
        let target = headers.iter().find(|h| !h.is_read).expect("an unread msg");
        let id = target.id.clone();
        db.mark_read(&id, true).unwrap();

        // Replay to the server.
        let mut p2 = ImapProvider::connect(&cfg).unwrap();
        let report = replay_with_provider(&db, "gm", &mut p2).unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(report.failed, 0);
        let _ = p2.logout();

        // Verify server-side: refetch (BODY.PEEK, doesn't set flags) → \Seen is set.
        let (path, uid) = db.message_imap_coords(&id).unwrap().unwrap();
        let mut p3 = ImapProvider::connect(&cfg).unwrap();
        p3.select(&path).unwrap();
        let fetched = p3.fetch_new(Some(uid - 1)).unwrap();
        let m = fetched.iter().find(|m| m.uid == uid).expect("msg present");
        assert!(m.is_seen, "server must report \\Seen after replay");
        let _ = p3.logout();
    }

    #[test]
    fn provider_failure_marks_op_failed_and_does_not_abort() {
        let db = temp_db();
        seed_message(&db);
        db.mark_read("acc:INBOX:7", true).unwrap();

        let mut provider = RecordingProvider {
            fail: true,
            ..Default::default()
        };
        let report = replay_with_provider(&db, "acc", &mut provider).unwrap();
        assert_eq!(report.applied, 0);
        assert_eq!(report.failed, 1);

        // The failed op is still surfaced as pending (undelivered) so the outbox
        // can show it, with the error and one attempt recorded.
        let pending = db.pending_operations("acc").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].status, "failed");
        assert_eq!(pending[0].attempts, 1);
        assert!(pending[0].last_error.is_some());
    }

    fn op(status: &str, attempts: i64, updated_at_epoch: i64) -> OperationRecord {
        op_not_before(status, attempts, updated_at_epoch, None)
    }

    fn op_not_before(
        status: &str,
        attempts: i64,
        updated_at_epoch: i64,
        not_before_epoch: Option<i64>,
    ) -> OperationRecord {
        OperationRecord {
            id: 1,
            account_id: "acc".into(),
            kind: "mark_read".into(),
            payload_json: "{}".into(),
            status: status.into(),
            attempts,
            last_error: None,
            updated_at_epoch,
            not_before_epoch,
            scheduled: false,
        }
    }

    #[test]
    fn due_policy() {
        let now = 1_000_000;
        // queued/running are always due (under the cap).
        assert!(due_for_attempt(&op("queued", 0, now), now));
        assert!(due_for_attempt(&op("running", 3, now), now));
        // failed with attempts left retries immediately after the first failure
        // (backoff(1) == 0)...
        assert!(due_for_attempt(&op("failed", 1, now), now));
        // ...but backs off after further failures (backoff(2) == 1s).
        assert!(!due_for_attempt(&op("failed", 2, now), now));
        assert!(due_for_attempt(&op("failed", 2, now - 1), now));
        // exhausted attempts are terminal.
        assert!(!due_for_attempt(
            &op("failed", MAX_ATTEMPTS, now - 10_000),
            now
        ));
        assert!(!due_for_attempt(&op("running", MAX_ATTEMPTS, now), now));
        // succeeded/cancelled/failed_permanent/unknown never run.
        assert!(!due_for_attempt(&op("succeeded", 0, now), now));
        assert!(!due_for_attempt(&op("cancelled", 0, now), now));
        assert!(!due_for_attempt(
            &op("failed_permanent", 3, now - 10_000),
            now
        ));
    }

    #[test]
    fn undo_send_window_gates_due_for_attempt() {
        // S25: a queued send with a durable not-yet-reached earliest-send time
        // is not due, however the status machine would otherwise read — this
        // is what keeps the sender from delivering during the undo window.
        let now = 1_000_000;
        assert!(
            !due_for_attempt(&op_not_before("queued", 0, now, Some(now + 1)), now),
            "not yet due: the window hasn't ended"
        );
        // The instant the window ends (now == not_before) it is due again —
        // and once it's in the past, unaffected (same as no not_before at all).
        assert!(due_for_attempt(
            &op_not_before("queued", 0, now, Some(now)),
            now
        ));
        assert!(due_for_attempt(
            &op_not_before("queued", 0, now, Some(now - 1)),
            now
        ));
        // `held` (the crash-safety hold) is never picked up by the normal
        // executor, whatever `not_before_epoch` says — only an explicit
        // send-now/undo call moves it.
        assert!(!due_for_attempt(
            &op_not_before("held", 0, now, Some(now - 100)),
            now
        ));
    }

    #[test]
    fn failure_status_is_terminal_only_at_cap() {
        assert_eq!(failure_status(1), "failed", "early failures stay retryable");
        assert_eq!(failure_status(MAX_ATTEMPTS - 1), "failed");
        assert_eq!(
            failure_status(MAX_ATTEMPTS),
            "failed_permanent",
            "the cap is terminal"
        );
        assert_eq!(failure_status(MAX_ATTEMPTS + 5), "failed_permanent");
    }

    #[test]
    fn crash_at_ceiling_is_retired_to_failed_permanent() {
        // An op stuck `running` at the attempt cap after a crash is retired to the
        // terminal `failed_permanent` — visible in the outbox, never retried, and
        // NOT counted in `report.failed` (it didn't fail an attempt this run).
        let db = temp_db();
        seed_message(&db); // acc:INBOX:7
        db.mark_read("acc:INBOX:7", true).unwrap();
        let id = db.pending_operations("acc").unwrap()[0].id;

        // Each `running` transition bumps attempts — model a crash on every attempt
        // up to the ceiling (leaves status `running`, attempts == MAX_ATTEMPTS).
        for _ in 0..MAX_ATTEMPTS {
            db.set_operation_status(id, "running", None).unwrap();
        }

        let mut provider = RecordingProvider::default();
        let report = replay_with_provider(&db, "acc", &mut provider).unwrap();
        assert_eq!(report.applied, 0, "terminal op is not applied");
        assert_eq!(report.failed, 0, "retired ops are not counted as failed");

        let pending = db.pending_operations("acc").unwrap();
        assert_eq!(pending.len(), 1, "still surfaced in the outbox");
        assert_eq!(pending[0].status, "failed_permanent");
        assert!(
            !due_for_attempt(&pending[0], now_epoch()),
            "a permanently-failed op is never due again"
        );
    }

    #[test]
    fn execution_failure_at_cap_becomes_failed_permanent() {
        // Exercise the EXECUTION arm (not crash recovery): an op at attempts == cap-1
        // that fails one more real attempt must land in `failed_permanent` and be
        // counted in `report.failed`; a subsequent run does nothing.
        let db = temp_db();
        seed_message(&db);
        db.mark_read("acc:INBOX:7", true).unwrap();
        let id = db.pending_operations("acc").unwrap()[0].id;

        // Pre-load attempts to MAX_ATTEMPTS-1 (still due), status `running`.
        for _ in 0..(MAX_ATTEMPTS - 1) {
            db.set_operation_status(id, "running", None).unwrap();
        }

        let mut provider = RecordingProvider {
            fail: true,
            ..Default::default()
        };
        let report = replay_with_provider(&db, "acc", &mut provider).unwrap();
        assert_eq!(report.failed, 1, "the failing attempt is counted");
        let after = db.pending_operations("acc").unwrap();
        assert_eq!(after[0].status, "failed_permanent");
        assert_eq!(after[0].attempts, MAX_ATTEMPTS);

        // A further run neither applies nor re-fails it.
        let report2 = replay_with_provider(&db, "acc", &mut provider).unwrap();
        assert_eq!(report2.applied, 0);
        assert_eq!(report2.failed, 0);
    }

    #[test]
    fn pre_existing_failed_at_cap_self_heals_preserving_error() {
        // An op left `failed` at the cap by an older build (before failed_permanent
        // existed) is normalised to `failed_permanent` on the next run, WITHOUT
        // losing its recorded error.
        let db = temp_db();
        seed_message(&db);
        db.mark_read("acc:INBOX:7", true).unwrap();
        let id = db.pending_operations("acc").unwrap()[0].id;

        // attempts -> cap (via running bumps), then land it back in `failed` with a
        // real error, as an old build would have left it.
        for _ in 0..MAX_ATTEMPTS {
            db.set_operation_status(id, "running", None).unwrap();
        }
        db.set_operation_status(id, "failed", Some("smtp down long ago"))
            .unwrap();

        // A non-failing provider: the op is retired before the execution loop, so
        // the provider is never invoked for it.
        let mut provider = RecordingProvider::default();
        replay_with_provider(&db, "acc", &mut provider).unwrap();

        let after = db.pending_operations("acc").unwrap();
        assert_eq!(after[0].status, "failed_permanent");
        assert_eq!(
            after[0].last_error.as_deref(),
            Some("smtp down long ago"),
            "the real failure reason must be preserved on retirement"
        );
    }

    #[test]
    fn replayer_ignores_cancelled_ops() {
        // An op cancelled by a UIDVALIDITY purge must be invisible to the
        // replayer: not applied, not failed, no attempt consumed.
        let db = temp_db();
        seed_message(&db); // acc:INBOX:7 in acc:INBOX
        db.mark_read("acc:INBOX:7", true).unwrap();
        assert_eq!(
            db.cancel_operations_for_purged_mailbox("acc", "acc:INBOX", "INBOX", "cancelled: test")
                .unwrap(),
            1
        );

        let mut provider = RecordingProvider::default();
        let report = replay_with_provider(&db, "acc", &mut provider).unwrap();
        assert_eq!(report.applied, 0);
        assert_eq!(report.failed, 0);
        assert!(
            provider.calls.is_empty(),
            "no provider call for a cancelled op"
        );
        assert!(db.pending_operations("acc").unwrap().is_empty());
    }

    #[test]
    fn replay_config_debug_redacts_password() {
        let cfg: ReplayConfig = serde_json::from_str(
            r#"{"account_id":"acc","host":"imap.x","port":993,
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
    fn backoff_grows_then_caps() {
        assert_eq!(backoff_seconds(1), 0);
        assert_eq!(backoff_seconds(2), 1);
        assert_eq!(backoff_seconds(4), 9);
        assert_eq!(backoff_seconds(1_000_000), 3600); // capped
    }

    #[test]
    fn failed_op_is_retried_and_can_succeed() {
        let db = temp_db();
        seed_message(&db);
        db.mark_read("acc:INBOX:7", true).unwrap();

        // First run fails (transient outage).
        let mut down = RecordingProvider {
            fail: true,
            ..Default::default()
        };
        assert_eq!(
            replay_with_provider(&db, "acc", &mut down).unwrap().failed,
            1
        );

        // Next run (provider recovered): backoff(1) == 0, so it retries and lands.
        let mut up = RecordingProvider {
            fail: false,
            ..Default::default()
        };
        let report = replay_with_provider(&db, "acc", &mut up).unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(up.calls, vec![("INBOX".to_string(), 7, true)]);
        assert!(db.pending_operations("acc").unwrap().is_empty());
    }

    // #3457: a flag op waiting out its backoff must not be overtaken by the
    // user's newer opposite op, or it replays later and flips the server back.
    #[test]
    fn a_newer_flag_op_does_not_overtake_an_older_one_waiting_on_backoff() {
        let db = temp_db();
        seed_message(&db);
        db.set_flagged("acc:INBOX:7", true).unwrap();
        let mut down = RecordingProvider {
            fail: true,
            ..Default::default()
        };
        replay_with_provider(&db, "acc", &mut down).unwrap(); // attempts 1 (no wait)
        replay_with_provider(&db, "acc", &mut down).unwrap(); // attempts 2 (1s wait)
        db.set_flagged("acc:INBOX:7", false).unwrap();

        let mut up = RecordingProvider::default();
        replay_with_provider(&db, "acc", &mut up).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1200));
        replay_with_provider(&db, "acc", &mut up).unwrap();

        let states: Vec<bool> = up.flag_calls.iter().map(|c| c.2).collect();
        assert_eq!(
            states.last(),
            Some(&false),
            "the user's last intent (unstar) must be the last STORE, got {states:?}"
        );
        assert!(db.pending_operations("acc").unwrap().is_empty());
    }

    #[test]
    fn mark_all_read_legacy_payload_keeps_the_whole_mailbox_store() {
        let db = temp_db();
        seed_message(&db);
        let mut p = RecordingProvider::default();
        let payload = serde_json::json!({ "mailbox_id": "acc:INBOX" }).to_string();
        apply_operation(&db, &mut p, "acc", 1, 0, "mark_all_read", &payload).unwrap();
        assert_eq!(p.mark_all_calls, vec!["INBOX".to_string()]);
        assert!(p.mark_seen_uids_calls.is_empty());
    }

    #[test]
    fn mark_all_read_replays_one_bulk_store() {
        let db = temp_db();
        seed_message(&db);
        db.set_sync_cursor(
            "acc",
            "acc:INBOX",
            &SyncCursor {
                uid_validity: Some(9),
                uid_next: Some(8),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();

        let affected = db.mark_all_read("acc", "acc:INBOX").unwrap();
        assert_eq!(affected, 1);
        // Exactly one op enqueued (not one per message).
        let pending = db.pending_operations("acc").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, "mark_all_read");

        let mut provider = RecordingProvider {
            fail: false,
            select_uid_validity: Some(9),
            ..Default::default()
        };
        let report = replay_with_provider(&db, "acc", &mut provider).unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(
            provider.mark_seen_uids_calls,
            vec![("INBOX".to_string(), "7".to_string())]
        );
        assert!(db.pending_operations("acc").unwrap().is_empty());

        // Nothing left unread → no-op, no new op enqueued.
        assert_eq!(db.mark_all_read("acc", "acc:INBOX").unwrap(), 0);
        assert!(db.pending_operations("acc").unwrap().is_empty());
    }

    #[test]
    fn mark_all_read_replay_spares_messages_that_arrived_after_enqueue() {
        // Mark All as Read pressed over UIDs 1-5; UID 6 arrives (unseen)
        // before the op replays. The replay must store \Seen on exactly the
        // snapshot, never on a range that covers UID 6 (FP27).
        let db = temp_db();
        seed_message(&db); // account + INBOX (+ acc:INBOX:7, purged below)
        db.delete_messages_for_mailbox("acc", "acc:INBOX").unwrap();
        for uid in 1..=5 {
            seed_msg(
                &db,
                &format!("acc:INBOX:{uid}"),
                "acc:INBOX",
                uid,
                &format!("<{uid}@x>"),
            );
        }
        db.set_sync_cursor(
            "acc",
            "acc:INBOX",
            &SyncCursor {
                uid_validity: Some(9),
                uid_next: Some(6),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
        assert_eq!(db.mark_all_read("acc", "acc:INBOX").unwrap(), 5);
        seed_msg(&db, "acc:INBOX:6", "acc:INBOX", 6, "<6@x>");

        let mut p = RecordingProvider {
            select_uid_validity: Some(9),
            ..Default::default()
        };
        let report = replay_with_provider(&db, "acc", &mut p).unwrap();
        assert_eq!(report.applied, 1);
        assert!(p.mark_all_calls.is_empty(), "no UID STORE 1:* at replay");
        assert_eq!(
            p.mark_seen_uids_calls,
            vec![("INBOX".to_string(), "1:5".to_string())]
        );
    }

    #[test]
    fn mark_all_read_replay_refuses_a_changed_uid_space() {
        let db = temp_db();
        seed_message(&db); // acc:INBOX:7, unread
        db.set_sync_cursor(
            "acc",
            "acc:INBOX",
            &SyncCursor {
                uid_validity: Some(9),
                uid_next: Some(8),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
        assert_eq!(db.mark_all_read("acc", "acc:INBOX").unwrap(), 1);
        let mut p = RecordingProvider {
            select_uid_validity: Some(10),
            ..Default::default()
        };
        let report = replay_with_provider(&db, "acc", &mut p).unwrap();
        assert_eq!(report.failed, 1);
        assert!(p.mark_all_calls.is_empty());
        assert!(p.mark_seen_uids_calls.is_empty());
    }

    fn seed_msg(db: &Database, id: &str, mailbox_id: &str, uid: i64, rfc: &str) {
        db.upsert_message(&NewMessage {
            id: id.into(),
            account_id: "acc".into(),
            mailbox_id: mailbox_id.into(),
            provider_uid: Some(uid),
            rfc822_message_id: Some(rfc.into()),
            subject: Some("Hi".into()),
            from_name: None,
            from_address: Some("a@x".into()),
            sent_at: Some(1000),
            snippet: None,
            size_bytes: Some(10),
            has_attachments: false,
            attachments: None,
            is_read: false,
            is_starred: false,
            addresses: vec![],
            text_body: None,
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: vec![],
        })
        .unwrap();
    }

    #[test]
    fn flag_op_for_a_since_deleted_message_succeeds() {
        // Flag a message, then delete it (row removed) before replay. The flag
        // must still reach the server, on the SOURCE coordinates and BEFORE the
        // move that carries the flags along (FP23).
        let db = temp_db();
        seed_message(&db); // acc:INBOX:7
        db.set_flagged("acc:INBOX:7", true).unwrap();
        db.delete_message("acc:INBOX:7").unwrap(); // auto-creates Trash + move; deletes row
        assert!(db.message_imap_coords("acc:INBOX:7").unwrap().is_none());

        let mut p = RecordingProvider::default();
        let report = replay_with_provider(&db, "acc", &mut p).unwrap();
        assert_eq!(report.failed, 0, "the flag op must not fail");
        assert_eq!(report.applied, 2, "flag + move both applied");
        assert_eq!(p.flag_calls, vec![("INBOX".to_string(), 7, true)]);
        assert_eq!(p.order, vec!["store_flagged", "move_message"]);
        assert!(db.pending_operations("acc").unwrap().is_empty());
    }

    #[test]
    fn mark_read_op_for_a_since_moved_message_stores_before_the_move() {
        let db = temp_db();
        seed_message(&db); // acc:INBOX:7
        db.mark_read("acc:INBOX:7", true).unwrap();
        db.delete_message("acc:INBOX:7").unwrap();

        let mut p = RecordingProvider::default();
        let report = replay_with_provider(&db, "acc", &mut p).unwrap();
        assert_eq!(report.failed, 0);
        assert_eq!(report.applied, 2);
        assert_eq!(p.calls, vec![("INBOX".to_string(), 7, true)]);
        assert_eq!(p.order, vec!["store_seen", "move_message"]);
    }

    #[test]
    fn keyword_op_for_a_since_moved_message_stores_before_the_move() {
        let db = temp_db();
        seed_message(&db); // acc:INBOX:7
        db.set_message_keyword("acc:INBOX:7", "Work", true).unwrap();
        db.delete_message("acc:INBOX:7").unwrap();

        let mut p = RecordingProvider::default();
        let report = replay_with_provider(&db, "acc", &mut p).unwrap();
        assert_eq!(report.failed, 0);
        assert_eq!(report.applied, 2);
        assert_eq!(
            p.keyword_calls,
            vec![("INBOX".to_string(), 7, "Work".to_string(), true)]
        );
        assert_eq!(p.order, vec!["store_keyword", "move_message"]);
    }

    #[test]
    fn state_op_with_neither_a_row_nor_a_snapshot_is_still_a_no_op() {
        // An op queued by an older build (no src_path/uid in the payload) for
        // a message that is gone: nothing to address, so it clears quietly.
        let db = temp_db();
        seed_message(&db);
        db.set_flagged("acc:INBOX:7", true).unwrap();
        db.delete_messages_for_mailbox("acc", "acc:INBOX").unwrap();

        let mut p = RecordingProvider::default();
        let report = replay_with_provider(&db, "acc", &mut p).unwrap();
        assert_eq!(report.failed, 0);
        assert!(p.flag_calls.is_empty());
        assert!(db.pending_operations("acc").unwrap().is_empty());
    }

    // -- S42: `keyword` op replay (Task 5) ------------------------------

    #[test]
    fn keyword_op_stored_clears_the_op() {
        let db = temp_db();
        seed_message(&db); // acc:INBOX:7, write-enabled
        db.set_message_keyword("acc:INBOX:7", "Work", true).unwrap();
        assert_eq!(db.pending_operations("acc").unwrap().len(), 1);

        let mut provider = RecordingProvider::default();
        let report = replay_with_provider(&db, "acc", &mut provider).unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(report.failed, 0);
        assert!(db.pending_operations("acc").unwrap().is_empty());
        assert_eq!(
            provider.keyword_calls,
            vec![("INBOX".to_string(), 7, "Work".to_string(), true)]
        );
        // Stored — origin stays 'server', never demoted.
        let map = db
            .keywords_for_messages(&["acc:INBOX:7".to_string()])
            .unwrap();
        assert_eq!(map.get("acc:INBOX:7").cloned().unwrap(), vec!["Work"]);
    }

    #[test]
    fn keyword_op_not_permanent_clears_the_op_and_demotes_to_local() {
        let db = temp_db();
        seed_message(&db);
        db.set_message_keyword("acc:INBOX:7", "Work", true).unwrap();

        let mut provider = RecordingProvider {
            keyword_outcome: Some(KeywordStoreOutcome::NotPermanent),
            ..Default::default()
        };
        let report = replay_with_provider(&db, "acc", &mut provider).unwrap();
        assert_eq!(report.applied, 1, "a refused keyword is not an op failure");
        assert_eq!(report.failed, 0);
        assert!(db.pending_operations("acc").unwrap().is_empty());
        // The row survives, demoted to origin='local' (kept on this Mac only):
        // proved indirectly, since `replace_server_keywords` never removes a
        // local-origin row even when the server reports nothing at all.
        let map = db
            .keywords_for_messages(&["acc:INBOX:7".to_string()])
            .unwrap();
        assert_eq!(map.get("acc:INBOX:7").cloned().unwrap(), vec!["Work"]);
        db.replace_server_keywords("acc:INBOX:7", &[]).unwrap();
        let map = db
            .keywords_for_messages(&["acc:INBOX:7".to_string()])
            .unwrap();
        assert_eq!(
            map.get("acc:INBOX:7").cloned().unwrap(),
            vec!["Work"],
            "a local-origin row must survive an empty server report"
        );
    }

    #[test]
    fn keyword_add_then_remove_before_replay_is_not_resurrected_by_demotion() {
        // Add then remove queued before replay, on a mailbox that refuses
        // keywords: the removed tag must stay removed (demotion must never
        // INSERT a local row).
        let db = temp_db();
        seed_message(&db);
        db.set_message_keyword("acc:INBOX:7", "Work", true).unwrap();
        db.set_message_keyword("acc:INBOX:7", "Work", false)
            .unwrap();
        assert_eq!(db.pending_operations("acc").unwrap().len(), 2);

        let mut provider = RecordingProvider {
            keyword_outcome: Some(KeywordStoreOutcome::NotPermanent),
            ..Default::default()
        };
        let report = replay_with_provider(&db, "acc", &mut provider).unwrap();
        assert_eq!(report.failed, 0);
        assert!(db.pending_operations("acc").unwrap().is_empty());
        let map = db
            .keywords_for_messages(&["acc:INBOX:7".to_string()])
            .unwrap();
        assert!(
            map.get("acc:INBOX:7").is_none_or(|v| v.is_empty()),
            "removed tag was resurrected: {map:?}"
        );
    }

    #[test]
    fn keyword_op_provider_error_leaves_it_for_retry() {
        let db = temp_db();
        seed_message(&db);
        db.set_message_keyword("acc:INBOX:7", "Work", true).unwrap();

        let mut provider = RecordingProvider {
            fail: true,
            ..Default::default()
        };
        let report = replay_with_provider(&db, "acc", &mut provider).unwrap();
        assert_eq!(report.applied, 0);
        assert_eq!(report.failed, 1);
        let pending = db.pending_operations("acc").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].status, "failed");
        assert_eq!(pending[0].attempts, 1);
        assert!(pending[0].last_error.is_some());
    }

    #[test]
    fn a_move_waits_for_a_failing_flag_op_stamped_with_its_coordinates() {
        // Audit #3451: star, then archive. The star fails once; the move must not
        // overtake it, or the retry has no message to star and the change is lost.
        let db = temp_db();
        seed_message(&db);
        db.set_flagged("acc:INBOX:7", true).unwrap();
        db.archive_message("acc:INBOX:7").unwrap();
        let mut p = RecordingProvider {
            fail_flag_once: true,
            ..Default::default()
        };
        let first = replay_with_provider(&db, "acc", &mut p).unwrap();
        assert!(
            p.move_calls.is_empty(),
            "the move must wait for the failed flag op"
        );
        assert_eq!(first.failed, 1);
        assert_eq!(first.blocked, 1);
        // backoff(1) is 0s, so the next pass retries the flag op first, then the move.
        for _ in 0..2 {
            replay_with_provider(&db, "acc", &mut p).unwrap();
        }
        assert_eq!(
            p.move_calls.len(),
            1,
            "the move goes after the flag succeeded"
        );
        assert_eq!(
            p.order.first().copied(),
            Some("store_flagged"),
            "{:?}",
            p.order
        );
    }

    // #3457: the older op failing in THIS pass must hold the newer opposite
    // op for the same target, or the retry flips the server back.
    #[test]
    fn a_newer_flag_op_waits_when_the_older_one_fails_in_the_same_pass() {
        let db = temp_db();
        seed_message(&db);
        db.set_flagged("acc:INBOX:7", true).unwrap();
        db.set_flagged("acc:INBOX:7", false).unwrap();

        let mut p = RecordingProvider {
            fail_flag_once: true,
            ..Default::default()
        };
        for _ in 0..3 {
            replay_with_provider(&db, "acc", &mut p).unwrap();
        }
        let states: Vec<bool> = p.flag_calls.iter().map(|c| c.2).collect();
        assert_eq!(
            states.last(),
            Some(&false),
            "the user's last intent (unstar) must be the last STORE, got {states:?}"
        );
    }

    fn arrival_70() -> NewMessage {
        NewMessage {
            id: "acc:Archive:70".into(),
            account_id: "acc".into(),
            mailbox_id: "acc:Archive".into(),
            provider_uid: Some(70),
            rfc822_message_id: Some("<7@x>".into()),
            subject: Some("Hi".into()),
            from_name: None,
            from_address: Some("a@x".into()),
            sent_at: Some(1000),
            snippet: None,
            size_bytes: Some(10),
            has_attachments: false,
            attachments: None,
            is_read: false,
            is_starred: false,
            addresses: vec![],
            text_body: None,
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: vec![],
        }
    }

    // #3438: tag then move before the keyword op replays; the server refuses
    // the keyword. The row is gone, so the tag must ride the move's stash.
    #[test]
    fn a_refused_tag_on_an_already_moved_message_follows_it_to_the_destination() {
        let db = temp_db();
        seed_message(&db);
        seed_mailbox(&db, "acc:Archive", "Archive", "archive");
        db.set_message_keyword("acc:INBOX:7", "Work", true).unwrap();
        db.archive_message("acc:INBOX:7").unwrap();

        let mut p = RecordingProvider {
            keyword_outcome: Some(KeywordStoreOutcome::NotPermanent),
            ..Default::default()
        };
        replay_with_provider(&db, "acc", &mut p).unwrap();
        assert_eq!(p.keyword_calls.len(), 1, "keyword replays before the move");

        db.upsert_message(&arrival_70()).unwrap();
        let map = db
            .keywords_for_messages(&["acc:Archive:70".to_string()])
            .unwrap();
        assert_eq!(map.get("acc:Archive:70").cloned().unwrap(), vec!["Work"]);
    }

    #[test]
    fn a_refused_tag_removed_before_the_move_is_not_resurrected() {
        let db = temp_db();
        seed_message(&db);
        seed_mailbox(&db, "acc:Archive", "Archive", "archive");
        db.set_message_keyword("acc:INBOX:7", "Work", true).unwrap();
        db.set_message_keyword("acc:INBOX:7", "Work", false)
            .unwrap();
        db.archive_message("acc:INBOX:7").unwrap();
        let mut p = RecordingProvider {
            keyword_outcome: Some(KeywordStoreOutcome::NotPermanent),
            ..Default::default()
        };
        replay_with_provider(&db, "acc", &mut p).unwrap();
        db.upsert_message(&arrival_70()).unwrap();
        let map = db
            .keywords_for_messages(&["acc:Archive:70".to_string()])
            .unwrap();
        assert!(
            map.get("acc:Archive:70").is_none_or(|v| v.is_empty()),
            "removed tag was resurrected: {map:?}"
        );
    }

    fn seed_mailbox(db: &Database, id: &str, path: &str, role: &str) {
        db.upsert_mailbox(&Mailbox {
            id: id.into(),
            account_id: "acc".into(),
            path: path.into(),
            name: path.into(),
            role: Some(role.into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
    }

    // --- GreenMail e2e helpers -------------------------------------------
    // Each e2e test provisions its OWN GreenMail account (delivery to an
    // unknown address auto-creates it with password == address under
    // `-Dgreenmail.setup.test.all`; IMAP login alone does not), and appends
    // its own uniquely-identified probe. Per-account isolation makes the
    // suite parallel-safe with no cross-test locking. The host is
    // GREENMAIL_HOST-overridable for CI service containers; this mirrors
    // tests/common/mod.rs, which unit tests inside the crate cannot import.

    static PROBE_SEQ: AtomicU64 = AtomicU64::new(0);

    fn gm_host() -> String {
        std::env::var("GREENMAIL_HOST").unwrap_or_else(|_| "127.0.0.1".into())
    }

    /// Provision (idempotently) a per-test account via a bootstrap SMTP
    /// delivery and return its address (password == address).
    fn gm_provision(test: &str) -> String {
        use std::io::{BufRead, BufReader, Write};
        let email = format!("e2e-{test}@local.test");
        let stream = std::net::TcpStream::connect((gm_host().as_str(), 3025u16))
            .expect("connect GreenMail SMTP");
        let mut reader = BufReader::new(stream.try_clone().expect("clone SMTP stream"));
        let mut writer = stream;
        let mut expect = |code: &str| loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read SMTP reply");
            assert!(line.starts_with(code), "SMTP expected {code}, got: {line}");
            if line.as_bytes().get(3) == Some(&b' ') {
                break;
            }
        };
        expect("220");
        writer.write_all(b"HELO castellum-e2e\r\n").unwrap();
        expect("250");
        writer
            .write_all(b"MAIL FROM:<bootstrap@local.test>\r\n")
            .unwrap();
        expect("250");
        writer
            .write_all(format!("RCPT TO:<{email}>\r\n").as_bytes())
            .unwrap();
        expect("250");
        writer.write_all(b"DATA\r\n").unwrap();
        expect("354");
        writer
            .write_all(b"Subject: castellum e2e account bootstrap\r\n\r\nbootstrap\r\n.\r\n")
            .unwrap();
        expect("250");
        writer.write_all(b"QUIT\r\n").unwrap();
        // Read the closing 221 so the server never sees a half-closed
        // connection with an unread reply (some stacks RST those).
        expect("221");
        email
    }

    fn gm_cfg(user: &str) -> castellum_providers::ImapConfig {
        castellum_providers::ImapConfig {
            host: gm_host(),
            port: 3143,
            username: user.into(),
            password: user.into(),
            security: castellum_providers::TransportSecurity::Plaintext,
            allow_invalid_certs: false,
        }
    }

    /// Independent verification oracle: a raw `Session` (not `ImapProvider`,
    /// which is what these tests exercise).
    fn gm_raw(user: &str) -> castellum_provider_imap::session::Session<std::net::TcpStream> {
        let cfg = gm_cfg(user);
        let tcp = std::net::TcpStream::connect((cfg.host.as_str(), cfg.port)).unwrap();
        let mut session =
            castellum_provider_imap::session::Session::connect(tcp).expect("greeting");
        session.login(&cfg.username, &cfg.password).expect("login");
        session
    }

    /// Append a uniquely-identified probe to the account's INBOX (dated far in
    /// the future so it sorts to the top) and return (subject, message_id).
    fn gm_append_probe(user: &str, tag: &str) -> (String, String) {
        let seq = PROBE_SEQ.fetch_add(1, Ordering::Relaxed);
        let subject = format!("castellum-probe-{tag}-{}-{}", std::process::id(), seq);
        let msgid = format!("<{subject}@test>");
        let msg = format!(
            "Date: Sat, 01 Jan 2099 00:00:00 +0000\r\nMessage-ID: {msgid}\r\n\
             From: probe@test\r\nTo: {user}\r\nSubject: {subject}\r\n\r\nprobe body\r\n"
        );
        let mut s = gm_raw(user);
        s.append("INBOX", &[], msg.as_bytes()).unwrap();
        let _ = s.logout();
        (subject, msgid)
    }

    /// Sync the account's INBOX into `db` (account "gm") and return the id of
    /// the probe with the given subject.
    fn gm_sync_and_find(db: &Database, user: &str, subject: &str) -> String {
        use castellum_provider_imap::ImapProvider;
        let mut p = ImapProvider::connect(&gm_cfg(user)).unwrap();
        crate::sync::sync_with_provider(
            db,
            "gm",
            user,
            Some(&["INBOX".to_string()]),
            &mut p,
            "test-host",
        )
        .unwrap();
        let _ = p.logout();
        // These e2e tests exercise real mutations, so leave evaluation mode.
        db.set_account_write_enabled("gm", true).unwrap();
        db.list_message_headers("gm", "gm:INBOX", 500, 0)
            .unwrap()
            .iter()
            .find(|h| h.subject.as_deref() == Some(subject))
            .map(|h| h.id.clone())
            .expect("probe message present after sync")
    }

    #[test]
    fn destructive_ops_enqueue_and_replay() {
        let db = temp_db();
        seed_message(&db); // acc:INBOX:7
        seed_msg(&db, "acc:INBOX:8", "acc:INBOX", 8, "<8@x>");
        seed_mailbox(&db, "acc:Trash", "Trash", "trash");

        // Flag msg 7 (row stays); delete msg 8 → move to Trash (row removed).
        db.set_flagged("acc:INBOX:7", true).unwrap();
        db.delete_message("acc:INBOX:8").unwrap();
        assert!(
            db.message_imap_coords("acc:INBOX:8").unwrap().is_none(),
            "deleted message leaves the source view immediately"
        );

        let mut p = RecordingProvider::default();
        let report = replay_with_provider(&db, "acc", &mut p).unwrap();
        assert_eq!(report.applied, 2);
        assert_eq!(p.flag_calls, vec![("INBOX".to_string(), 7, true)]);
        assert_eq!(
            p.move_calls,
            vec![(
                "INBOX".to_string(),
                8,
                "<8@x>".to_string(),
                "Trash".to_string()
            )]
        );
        assert!(db.pending_operations("acc").unwrap().is_empty());

        // Empty Trash: seed a Trash message + a REAL sync cursor, empty,
        // replay → one expunge of exactly the snapshotted UID. A `0`/`-1`
        // sentinel UIDVALIDITY must never pass the replay-time check (PR
        // #165 review point 2), so the happy path needs a real positive
        // cursor that matches the provider mock's `select_uid_validity`.
        seed_msg(&db, "acc:Trash:1", "acc:Trash", 1, "<t1@x>");
        db.set_sync_cursor(
            "acc",
            "acc:Trash",
            &SyncCursor {
                uid_validity: Some(9),
                uid_next: Some(2),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
        assert_eq!(db.empty_trash("acc").unwrap(), 1);
        let mut p2 = RecordingProvider {
            select_uid_validity: Some(9),
            ..Default::default()
        };
        let r2 = replay_with_provider(&db, "acc", &mut p2).unwrap();
        assert_eq!(r2.applied, 1);
        assert_eq!(
            p2.expunge_calls,
            vec![("Trash".to_string(), "1".to_string())]
        );
    }

    /// End-to-end: delete a message (queues a `move` to Trash), then click
    /// Empty Trash before that move replays. PR #165 review point 1 — the
    /// rewritten `delete_permanent` op's replay must expunge the SOURCE
    /// mailbox (INBOX) and must never touch Trash at all for it.
    #[test]
    fn empty_trash_before_a_pending_move_replays_never_touches_trash_for_it() {
        let db = temp_db();
        seed_message(&db); // acc:INBOX:7, write-enabled
        seed_mailbox(&db, "acc:Trash", "Trash", "trash");
        db.set_sync_cursor(
            "acc",
            "acc:INBOX",
            &SyncCursor {
                uid_validity: Some(3),
                uid_next: Some(8),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();

        db.delete_message("acc:INBOX:7").unwrap(); // queues a `move` to Trash
        db.empty_trash("acc").unwrap(); // rewrites it into delete_permanent(INBOX)

        let kinds: Vec<String> = db
            .pending_operations("acc")
            .unwrap()
            .iter()
            .map(|o| o.kind.clone())
            .collect();
        assert!(!kinds.contains(&"move".to_string()), "{kinds:?}");
        assert!(kinds.contains(&"delete_permanent".to_string()), "{kinds:?}");
        assert!(kinds.contains(&"empty_trash".to_string()), "{kinds:?}");

        let mut p = RecordingProvider {
            select_uid_validity: Some(3), // matches INBOX's cursor
            ..Default::default()
        };
        let report = replay_with_provider(&db, "acc", &mut p).unwrap();
        assert_eq!(report.applied, 2, "both ops succeed");
        assert_eq!(
            p.expunge_calls,
            vec![("INBOX".to_string(), "7".to_string())],
            "only the SOURCE mailbox is ever expunged — Trash is never named, \
             because the message never actually reached it"
        );
    }

    /// Seeds a message that already lives in Trash (uid 9, UIDVALIDITY 5).
    fn seed_trashed_message(db: &Database) -> String {
        seed_message(db); // acc + INBOX, write-enabled
        seed_mailbox(db, "acc:Trash", "Trash", "trash");
        db.set_sync_cursor(
            "acc",
            "acc:Trash",
            &SyncCursor {
                uid_validity: Some(5),
                uid_next: Some(10),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
        db.upsert_message(&NewMessage {
            id: "acc:Trash:9".into(),
            account_id: "acc".into(),
            mailbox_id: "acc:Trash".into(),
            provider_uid: Some(9),
            rfc822_message_id: Some("<9@x>".into()),
            subject: Some("Bin me".into()),
            from_name: None,
            from_address: Some("a@x".into()),
            sent_at: Some(1000),
            snippet: None,
            size_bytes: Some(10),
            has_attachments: false,
            attachments: None,
            is_read: true,
            is_starred: false,
            addresses: vec![],
            text_body: Some("body".into()),
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: vec![],
        })
        .unwrap();
        "acc:Trash:9".into()
    }

    /// S73 / #2297: replaying a single-message permanent delete expunges
    /// exactly that UID in Trash, then (only then) drops the local row.
    #[test]
    fn permanent_delete_of_a_trashed_message_expunges_it_then_removes_the_row() {
        let db = temp_db();
        let id = seed_trashed_message(&db);
        db.delete_message_permanently(&id).unwrap();
        let mut p = RecordingProvider {
            select_uid_validity: Some(5),
            ..Default::default()
        };
        let report = replay_with_provider(&db, "acc", &mut p).unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(
            p.expunge_calls,
            vec![("Trash".to_string(), "9".to_string())]
        );
        assert_eq!(
            db.count_messages("acc", "acc:Trash").unwrap(),
            0,
            "row removed after success"
        );
    }

    /// S73 / #2297: a failed expunge must leave the message visible.
    #[test]
    fn a_failed_permanent_delete_leaves_the_message_visible() {
        let db = temp_db();
        let id = seed_trashed_message(&db);
        db.delete_message_permanently(&id).unwrap();
        let mut p = RecordingProvider {
            select_uid_validity: Some(5),
            fail: true,
            ..Default::default()
        };
        let report = replay_with_provider(&db, "acc", &mut p).unwrap();
        assert_eq!(report.applied, 0);
        assert_eq!(
            db.count_messages("acc", "acc:Trash").unwrap(),
            1,
            "row must survive a failure"
        );
    }

    /// Round-2 review HOLE, end-to-end: a move queued with NO source cursor
    /// (an interrupted first import), followed by a LATER sync writing a
    /// cursor (simulating a UIDVALIDITY reset the first-import path never
    /// detects), followed by Empty Trash. The rewritten `delete_permanent`
    /// must carry the UIDVALIDITY the move actually saw (`0`, none existed)
    /// — NOT the cursor written afterwards — so replay refuses and sends no
    /// STORE, rather than expunging whatever now occupies that UID under the
    /// new cursor's UID space.
    #[test]
    fn empty_trash_replay_refuses_a_delete_permanent_from_a_pre_reset_move_and_sends_no_store() {
        let db = temp_db();
        seed_message(&db); // acc:INBOX:7, write-enabled, NO sync cursor yet
        seed_mailbox(&db, "acc:Trash", "Trash", "trash");

        db.delete_message("acc:INBOX:7").unwrap(); // move queued — no cursor exists

        // A later sync completes and writes a cursor: simulates the server
        // having reset UIDVALIDITY between the interrupted first import and
        // this sync (the first-import path has no prior cursor to compare
        // against, so it never detects or cancels anything here).
        db.set_sync_cursor(
            "acc",
            "acc:INBOX",
            &SyncCursor {
                uid_validity: Some(99),
                uid_next: Some(3),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();

        db.empty_trash("acc").unwrap(); // rewrites the move into delete_permanent

        // Even if the server NOW reports UIDVALIDITY 99 for INBOX (a perfect
        // but coincidental match to the new cursor), the delete_permanent's
        // own snapshot must still be 0 (what the move saw), so the check
        // fails closed regardless of what the live server says.
        let mut p = RecordingProvider {
            select_uid_validity: Some(99),
            ..Default::default()
        };
        let report = replay_with_provider(&db, "acc", &mut p).unwrap();
        assert_eq!(
            report.failed, 1,
            "the delete_permanent must be refused, not applied"
        );
        assert!(
            p.expunge_calls.is_empty(),
            "no STORE/EXPUNGE may be sent for a UID captured under an unknown/stale UID space"
        );
        let pending = db.pending_operations("acc").unwrap();
        let dp = pending
            .iter()
            .find(|o| o.kind == "delete_permanent")
            .unwrap();
        assert_eq!(dp.status, "failed_permanent");
    }

    /// A message that lands in Trash AFTER the snapshot (another client, a
    /// server-side rule, a replay delayed hours) must never be touched by the
    /// expunge — it is simply not in the snapshotted UID set, no matter how
    /// wide the mailbox's live UID range is by the time replay runs.
    #[test]
    fn empty_trash_replay_never_widens_the_snapshot_uid_set() {
        let db = temp_db();
        let mut p = RecordingProvider {
            select_uid_validity: Some(7),
            ..Default::default()
        };
        let payload = serde_json::json!({
            "path": "Trash",
            "uid_validity": 7,
            "uids": "5,6",
        })
        .to_string();
        apply_operation(&db, &mut p, "acc", 1, 0, "empty_trash", &payload).unwrap();
        assert_eq!(
            p.expunge_calls,
            vec![("Trash".to_string(), "5,6".to_string())],
            "only the snapshotted UIDs are ever named, never 1:* or a wider set"
        );
    }

    /// A snapshot with zero server-known UIDs (Trash held only local-only
    /// rows) must succeed trivially without ever calling the provider — there
    /// is nothing to expunge and nothing to select.
    #[test]
    fn empty_trash_replay_with_empty_snapshot_never_calls_the_provider() {
        let db = temp_db();
        let mut p = RecordingProvider::default();
        let payload = serde_json::json!({
            "path": "Trash",
            "uid_validity": 7,
            "uids": "",
        })
        .to_string();
        apply_operation(&db, &mut p, "acc", 1, 0, "empty_trash", &payload).unwrap();
        assert!(p.expunge_calls.is_empty());
    }

    /// The UID space was reset since the snapshot (a UIDVALIDITY bump): UID 5
    /// may now name a completely different message. Replay must refuse rather
    /// than guess, and must not have expunged anything on the way there.
    #[test]
    fn empty_trash_replay_aborts_on_uidvalidity_mismatch_without_expunging() {
        let db = temp_db();
        let mut p = RecordingProvider {
            select_uid_validity: Some(99), // server now reports a different value
            ..Default::default()
        };
        let payload = serde_json::json!({
            "path": "Trash",
            "uid_validity": 7, // snapshot taken when UIDVALIDITY was 7
            "uids": "5,6",
        })
        .to_string();
        let err = apply_operation(&db, &mut p, "acc", 1, 0, "empty_trash", &payload)
            .expect_err("a UIDVALIDITY mismatch must refuse the replay");
        assert!(
            matches!(err, CoreError::NonRetryable(_)),
            "must be classified non-retryable, not a transient failure: {err:?}"
        );
        assert!(
            p.expunge_calls.is_empty(),
            "nothing may be expunged on a mismatch"
        );
    }

    /// follow-up: replay used to SELECT, compare, then call the
    /// unguarded `expunge_mailbox`, which SELECTs again without comparing. A
    /// mailbox rebuilt in between had the snapshot's UIDs (now other
    /// messages) permanently removed. The compare now belongs to the
    /// provider's own write: when IT reports a changed UID space, nothing is
    /// expunged even though a plain `select` still looks fine.
    #[test]
    fn empty_trash_and_permanent_delete_defer_the_uidvalidity_check_to_the_write_itself() {
        let db = temp_db();
        for kind in ["empty_trash", "delete_permanent"] {
            let mut p = RecordingProvider {
                select_uid_validity: Some(7),  // what a separate check would see
                rebuilt_before_write: Some(8), // what the write's SELECT sees
                ..Default::default()
            };
            let payload = serde_json::json!({
                "path": "Trash",
                "uid_validity": 7,
                "uids": "5,6",
            })
            .to_string();
            let err = apply_operation(&db, &mut p, "acc", 1, 0, kind, &payload)
                .expect_err("a mailbox rebuilt before the write must refuse");
            assert!(matches!(err, CoreError::NonRetryable(_)), "{kind}: {err:?}");
            assert!(err.to_string().contains("7 -> 8"), "{kind}: {err}");
            assert!(p.expunge_calls.is_empty(), "{kind}: nothing expunged");
        }
        // The matching case still expunges exactly the snapshot.
        let mut p = RecordingProvider {
            select_uid_validity: Some(7),
            ..Default::default()
        };
        let payload =
            serde_json::json!({"path": "Trash", "uid_validity": 7, "uids": "5,6"}).to_string();
        apply_operation(&db, &mut p, "acc", 1, 0, "empty_trash", &payload).unwrap();
        assert_eq!(
            p.expunge_calls,
            vec![("Trash".to_string(), "5,6".to_string())]
        );
    }

    /// Same window, Mark All as Read: the provider's guarded store decides.
    #[test]
    fn mark_all_read_defers_the_uidvalidity_check_to_the_write_itself() {
        let db = temp_db();
        seed_message(&db); // acc:INBOX:7, unread
        db.set_sync_cursor(
            "acc",
            "acc:INBOX",
            &SyncCursor {
                uid_validity: Some(9),
                uid_next: Some(8),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
        assert_eq!(db.mark_all_read("acc", "acc:INBOX").unwrap(), 1);
        let mut p = RecordingProvider {
            select_uid_validity: Some(9),
            rebuilt_before_write: Some(10),
            ..Default::default()
        };
        let report = replay_with_provider(&db, "acc", &mut p).unwrap();
        assert_eq!(report.failed, 1);
        assert!(p.mark_all_calls.is_empty());
        assert!(p.mark_seen_uids_calls.is_empty());
    }

    /// A `0`/`-1` UIDVALIDITY must never be treated as "matches the
    /// snapshot" just because the two sides happen to be numerically equal —
    /// `0` is `Database::empty_trash`'s own "no cursor yet" sentinel and `-1`
    /// is `ImapProvider::select`'s "server didn't report it" sentinel
    /// (provider-imap lib.rs). Either one means the UID space is genuinely
    /// unknown, so a bare `0 == 0` (or `-1 == -1`) must still refuse.
    #[test]
    fn empty_trash_replay_treats_a_zero_or_negative_uidvalidity_as_unknown_not_a_match() {
        let db = temp_db();

        // snapshot 0 (no cursor was ever recorded), current also 0.
        let mut p_zero = RecordingProvider {
            select_uid_validity: Some(0),
            ..Default::default()
        };
        let payload_zero = serde_json::json!({
            "path": "Trash",
            "uid_validity": 0,
            "uids": "5,6",
        })
        .to_string();
        let err = apply_operation(&db, &mut p_zero, "acc", 1, 0, "empty_trash", &payload_zero)
            .expect_err("0 == 0 must still be refused as unknown");
        assert!(matches!(err, CoreError::NonRetryable(_)), "{err:?}");
        assert!(p_zero.expunge_calls.is_empty());

        // snapshot -1 (a value that could only come from a server that never
        // reports UIDVALIDITY), current also -1.
        let mut p_neg = RecordingProvider {
            select_uid_validity: Some(-1),
            ..Default::default()
        };
        let payload_neg = serde_json::json!({
            "path": "Trash",
            "uid_validity": -1,
            "uids": "5,6",
        })
        .to_string();
        let err = apply_operation(&db, &mut p_neg, "acc", 1, 0, "empty_trash", &payload_neg)
            .expect_err("-1 == -1 must still be refused as unknown");
        assert!(matches!(err, CoreError::NonRetryable(_)), "{err:?}");
        assert!(p_neg.expunge_calls.is_empty());
    }

    /// A server with no UIDPLUS means `expunge_mailbox` refuses (PR #165
    /// review point 3) — that refusal must be classified `NonRetryable`, not
    /// the generic retryable `Sync`, since the server's capabilities won't
    /// change on a later attempt.
    #[test]
    fn empty_trash_replay_classifies_missing_uidplus_as_non_retryable() {
        let db = temp_db();
        let mut p = RecordingProvider {
            select_uid_validity: Some(7),
            fail_unsupported: true,
            ..Default::default()
        };
        let payload = serde_json::json!({
            "path": "Trash",
            "uid_validity": 7,
            "uids": "5,6",
        })
        .to_string();
        let err = apply_operation(&db, &mut p, "acc", 1, 0, "empty_trash", &payload)
            .expect_err("missing UIDPLUS must refuse");
        assert!(
            matches!(err, CoreError::NonRetryable(_)),
            "must be classified non-retryable: {err:?}"
        );
        assert!(p.expunge_calls.is_empty());
    }

    /// An op queued by a pre-fix build carries only `{"path": ...}` — no UID
    /// snapshot at all. Expunging "whatever Trash holds now" is exactly the
    /// data-loss bug this fix closes, so replay must refuse it outright
    /// rather than fall back to the old unsafe behaviour. It must also clear
    /// the mailbox's sync cursor, or "empty it again" does nothing: the old
    /// build already deleted the local Trash rows, and an incremental sync
    /// (resuming from the stored UIDNEXT) would never re-import them.
    #[test]
    fn empty_trash_replay_refuses_a_legacy_bare_path_payload_and_clears_the_cursor() {
        let db = temp_db();
        seed_message(&db); // creates account "acc", write-enabled
        seed_mailbox(&db, "acc:Trash", "Trash", "trash");
        db.set_sync_cursor(
            "acc",
            "acc:Trash",
            &SyncCursor {
                uid_validity: Some(9),
                uid_next: Some(50),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
        assert!(db.get_sync_cursor("acc", "acc:Trash").unwrap().is_some());

        let mut p = RecordingProvider::default();
        let payload = serde_json::json!({ "path": "Trash" }).to_string();
        let err = apply_operation(&db, &mut p, "acc", 1, 0, "empty_trash", &payload)
            .expect_err("a legacy payload must be refused");
        assert!(
            matches!(err, CoreError::NonRetryable(_)),
            "must be classified non-retryable — retrying a legacy op can't fix it: {err:?}"
        );
        assert!(p.expunge_calls.is_empty());
        assert!(
            db.get_sync_cursor("acc", "acc:Trash").unwrap().is_none(),
            "the cursor must be cleared so the next sync re-imports Trash"
        );
    }

    /// End-to-end through `replay_with_provider`: a UIDVALIDITY mismatch must
    /// land the op in the terminal `failed_permanent` state on the FIRST
    /// attempt (never `queued`/`failed` waiting on backoff), and a second run
    /// must not touch it again — matching how `failed_permanent` already
    /// behaves for attempt-exhausted ops.
    #[test]
    fn empty_trash_uidvalidity_mismatch_is_failed_permanent_on_first_attempt() {
        let db = temp_db();
        seed_message(&db); // acc:INBOX:7, write-enabled
        seed_mailbox(&db, "acc:Trash", "Trash", "trash");
        seed_msg(&db, "acc:Trash:1", "acc:Trash", 1, "<t1@x>");
        assert_eq!(db.empty_trash("acc").unwrap(), 1); // snapshot uid_validity = 0 (no cursor)

        let mut p = RecordingProvider {
            select_uid_validity: Some(42), // server now reports something else
            ..Default::default()
        };
        let report = replay_with_provider(&db, "acc", &mut p).unwrap();
        assert_eq!(report.failed, 1);
        assert!(p.expunge_calls.is_empty());
        let pending = db.pending_operations("acc").unwrap();
        assert_eq!(pending[0].status, "failed_permanent");
        assert_eq!(
            pending[0].attempts, 1,
            "no attempt-count ladder — immediate"
        );

        // A second run must not retry it or touch the provider again.
        let report2 = replay_with_provider(&db, "acc", &mut p).unwrap();
        assert_eq!(report2.applied, 0);
        assert_eq!(report2.failed, 0);
        assert!(p.expunge_calls.is_empty());
    }

    /// Real IMAP: a move (archive) is idempotent — replaying it never duplicates
    /// the message in the destination. Ignored by default (needs GreenMail).
    #[test]
    #[ignore = "requires a local GreenMail (scripts/local-imap) on 3025/3143"]
    fn greenmail_move_is_idempotent() {
        use castellum_provider_imap::ImapProvider;
        use castellum_providers::MailProvider;
        let user = gm_provision("replay-move");

        // Use a fresh, selectable destination folder on our own account, then
        // append our own message and archive THAT.
        let mut setup = gm_raw(&user);
        let _ = setup.delete("Archive");
        setup.create("Archive").unwrap();
        let _ = setup.logout();

        let (subject, _) = gm_append_probe(&user, "move");
        let db = temp_db();
        let target_id = gm_sync_and_find(&db, &user, &subject);

        db.archive_message(&target_id).unwrap(); // enqueue move (dest resolves/creates Archive)
        let op = db
            .pending_operations("gm")
            .unwrap()
            .into_iter()
            .find(|o| o.kind == "move")
            .expect("a move op");
        let payload: serde_json::Value = serde_json::from_str(&op.payload_json).unwrap();
        let src = payload["src_path"].as_str().unwrap().to_string();
        let uid = payload["uid"].as_i64().unwrap();
        let msgid = payload["message_id"].as_str().unwrap().to_string();
        let dest = payload["dest_path"].as_str().unwrap().to_string();
        assert!(
            !msgid.is_empty(),
            "need a Message-ID for the idempotency guard"
        );

        // Move once, then again: the second must be a no-op, not a duplicate.
        let mut prov = ImapProvider::connect(&gm_cfg(&user)).unwrap();
        prov.move_message(&src, uid, &msgid, &dest).unwrap();
        prov.move_message(&src, uid, &msgid, &dest).unwrap();
        let _ = prov.logout();

        let mut check = gm_raw(&user);
        check.select(&dest).unwrap();
        let found = check.uid_search_by_message_id(&msgid).unwrap();
        assert_eq!(found.len(), 1, "exactly one copy in {dest} after two moves");
        check.select("INBOX").unwrap();
        let still = check.uid_search_by_uid(uid as u32).unwrap();
        assert!(still.is_empty(), "source copy removed from INBOX");
        let _ = check.logout();
    }

    #[test]
    fn delete_autocreates_trash_when_missing() {
        let db = temp_db();
        seed_message(&db); // no Trash mailbox seeded
        db.delete_message("acc:INBOX:7").unwrap(); // succeeds by creating one

        // A Trash mailbox now exists locally (the provider creates the server
        // folder at replay time).
        let mbs = db.list_mailboxes("acc").unwrap();
        assert!(mbs.iter().any(|m| m.role.as_deref() == Some("trash")));
        // The message left the source view and a move op is queued to Trash.
        assert!(db.message_imap_coords("acc:INBOX:7").unwrap().is_none());
        let ops = db.pending_operations("acc").unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].kind, "move");
        assert!(ops[0].payload_json.contains("\"dest_path\":\"Trash\""));
    }

    /// Real IMAP: deleting on an account with no Trash creates one and moves there.
    #[test]
    #[ignore = "requires a local GreenMail (scripts/local-imap) on 3025/3143"]
    fn greenmail_delete_autocreates_trash() {
        use castellum_provider_imap::ImapProvider;
        use castellum_providers::MailProvider;
        let user = gm_provision("replay-del");

        // Start with no Trash folder on our own account.
        let mut setup = gm_raw(&user);
        let _ = setup.delete("Trash");
        let _ = setup.logout();

        // Append our own message, sync, and delete THAT.
        let (subject, _) = gm_append_probe(&user, "del");
        let db = temp_db();
        let target_id = gm_sync_and_find(&db, &user, &subject);

        db.delete_message(&target_id).unwrap(); // auto-creates a local Trash + move op
        let op = db
            .pending_operations("gm")
            .unwrap()
            .into_iter()
            .find(|o| o.kind == "move")
            .expect("a move op");
        let payload: serde_json::Value = serde_json::from_str(&op.payload_json).unwrap();
        let (src, uid, msgid, dest) = (
            payload["src_path"].as_str().unwrap().to_string(),
            payload["uid"].as_i64().unwrap(),
            payload["message_id"].as_str().unwrap().to_string(),
            payload["dest_path"].as_str().unwrap().to_string(),
        );
        assert_eq!(dest, "Trash");

        let mut prov = ImapProvider::connect(&gm_cfg(&user)).unwrap();
        prov.move_message(&src, uid, &msgid, &dest).unwrap(); // creates Trash + moves
        let _ = prov.logout();

        // Verify: Trash now exists and holds the message; INBOX no longer has it.
        let mut check = gm_raw(&user);
        check.select("Trash").unwrap();
        let found = check.uid_search_by_message_id(&msgid).unwrap();
        assert_eq!(found.len(), 1, "message landed in the auto-created Trash");
        check.select("INBOX").unwrap();
        assert!(check.uid_search_by_uid(uid as u32).unwrap().is_empty());
        let _ = check.logout();
    }

    /// Real IMAP (undo, #3022): archive, replay the move, undo, replay the undo -
    /// the message is back in the Inbox exactly once, found by the COPYUID the
    /// move recorded (no search), and a DECOY whose Message-ID merely contains
    /// the target's is never touched.
    #[test]
    #[ignore = "requires a local GreenMail (scripts/local-imap) on 3025/3143"]
    fn greenmail_archive_then_undo_round_trips_to_the_inbox() {
        use castellum_provider_imap::ImapProvider;
        let user = gm_provision("undo-roundtrip");
        // GreenMail answers DELETE of a mailbox that does not exist with an internal error that
        // closes the connection, so never delete here: CREATE if missing (an existing Archive
        // from an earlier run is harmless, every probe has its own Message-ID).
        let mut setup = gm_raw(&user);
        let _ = setup.create("Archive");
        let _ = setup.logout();

        let (subject, msgid) = gm_append_probe(&user, "undo");
        // A decoy in the Archive whose Message-ID CONTAINS the target's.
        let decoy_id = format!("<x{}>", msgid.trim_start_matches('<'));
        let decoy = format!(
            "Date: Sat, 01 Jan 2099 00:00:01 +0000\r\nMessage-ID: {decoy_id}\r\n\
             From: decoy@test\r\nTo: {user}\r\nSubject: decoy\r\n\r\ndecoy\r\n"
        );
        let mut s = gm_raw(&user);
        s.append("Archive", &[], decoy.as_bytes()).unwrap();
        let _ = s.logout();

        let db = temp_db();
        let target = gm_sync_and_find(&db, &user, &subject);
        let moved = db
            .move_messages(&[target], castellum_db::MoveTarget::Archive)
            .unwrap();
        assert_eq!(moved.operation_ids.len(), 1);

        let mut prov = ImapProvider::connect(&gm_cfg(&user)).unwrap();
        let report = replay_with_provider(&db, "gm", &mut prov).unwrap();
        assert_eq!(report.applied, 1, "{report:?}");
        let original: serde_json::Value = serde_json::from_str(
            &db.operation_payload(moved.operation_ids[0])
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let dest_uid = original["dest_uid"].as_i64();
        assert!(
            dest_uid.is_some(),
            "COPYUID recorded on the move: {original}"
        );

        // Undo, then replay the inverse.
        let undo = db.undo_moves(&moved.operation_ids).unwrap();
        assert_eq!(undo.queued, 1);
        let report = replay_with_provider(&db, "gm", &mut prov).unwrap();
        assert_eq!(report.applied, 1, "{report:?}");
        let _ = prov.logout();

        let mut check = gm_raw(&user);
        check.select("INBOX").unwrap();
        assert_eq!(
            check.uid_search_by_message_id(&msgid).unwrap().len(),
            1,
            "the message is back in the Inbox"
        );
        check.select("Archive").unwrap();
        assert_eq!(
            check.uid_search_by_message_id(&decoy_id).unwrap().len(),
            1,
            "the decoy was never moved"
        );
        assert_eq!(
            check
                .uid_search_by_uid(dest_uid.unwrap() as u32)
                .unwrap()
                .len(),
            0,
            "the Archive copy left"
        );
        let _ = check.logout();
    }

    /// Records `append` calls; every other write is untouched default.
    #[derive(Default)]
    struct AppendOnlyProvider {
        appends: Vec<(String, String, Vec<u8>)>,
    }

    impl MailProvider for AppendOnlyProvider {
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
        fn fetch_new(&mut self, _after: Option<i64>) -> Result<Vec<RemoteMessage>, ProviderError> {
            Ok(vec![])
        }
        fn logout(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }
        fn append(
            &mut self,
            path: &str,
            message_id: &str,
            raw: &[u8],
        ) -> Result<(), ProviderError> {
            self.appends
                .push((path.to_string(), message_id.to_string(), raw.to_vec()));
            Ok(())
        }
    }

    #[test]
    fn legacy_inline_sent_copy_payload_still_replays() {
        // Ops queued by pre-blob builds carry the bytes inline as raw_b64 and
        // must keep replaying after the upgrade — no blob row exists for them.
        let db = temp_db();
        let payload = serde_json::json!({
            "dest_path": "Sent",
            "message_id": "<legacy@x>",
            "raw_b64": BASE64.encode(b"legacy delivered bytes"),
        })
        .to_string();

        let mut provider = AppendOnlyProvider::default();
        apply_operation(&db, &mut provider, "acc", 424_242, 0, "sent_copy", &payload).unwrap();

        assert_eq!(provider.appends.len(), 1);
        let (path, msgid, raw) = &provider.appends[0];
        assert_eq!(path, "Sent");
        assert_eq!(msgid, "<legacy@x>");
        assert_eq!(raw, b"legacy delivered bytes");
    }

    #[test]
    fn sent_copy_without_inline_or_blob_bytes_fails_cleanly() {
        // A blob-referencing op whose bytes are gone must fail with a clear
        // error (recorded on the op), never append garbage.
        let db = temp_db();
        let payload = serde_json::json!({
            "dest_path": "Sent",
            "message_id": "<missing@x>",
        })
        .to_string();

        let mut provider = AppendOnlyProvider::default();
        let err = apply_operation(&db, &mut provider, "acc", 424_243, 0, "sent_copy", &payload)
            .unwrap_err();
        assert!(err.to_string().contains("no stored bytes"), "{err}");
        assert!(provider.appends.is_empty());
    }

    #[test]
    fn import_eml_replays_as_one_append_of_the_stored_bytes() {
        let db = temp_db();
        seed_message(&db); // account "acc" + mailbox "acc:INBOX", write-enabled
        let op_id = db
            .enqueue_eml_import("acc", "acc:INBOX", "<synth@x>", b"From: a@x\r\n\r\nbody")
            .unwrap();

        let mut provider = AppendOnlyProvider::default();
        let report = replay_with_provider(&db, "acc", &mut provider).unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(report.failed, 0);
        assert_eq!(provider.appends.len(), 1);
        assert_eq!(provider.appends[0].0, "INBOX");
        assert_eq!(provider.appends[0].1, "<synth@x>");
        assert_eq!(provider.appends[0].2, b"From: a@x\r\n\r\nbody");

        // The op is retired — a second replay pass does nothing.
        let again = replay_with_provider(&db, "acc", &mut provider).unwrap();
        assert_eq!(again.applied, 0);
        assert_eq!(provider.appends.len(), 1, "no duplicate APPEND on replay");

        let _ = op_id;
    }

    #[test]
    fn import_eml_stays_blocked_while_the_account_is_read_only() {
        let db = temp_db();
        seed_message(&db);
        db.set_account_write_enabled("acc", false).unwrap();
        db.enqueue_eml_import("acc", "acc:INBOX", "<x@x>", b"raw")
            .unwrap();

        let mut provider = AppendOnlyProvider::default();
        let report = replay_with_provider(&db, "acc", &mut provider).unwrap();
        assert_eq!(report.applied, 0);
        assert_eq!(report.blocked, 1);
        assert!(provider.appends.is_empty());
    }

    // ---: move / flag / read / keyword check UIDVALIDITY ------

    /// The single pending op's (status, last_error).
    fn only_pending(db: &Database) -> (String, String) {
        let pending = db.pending_operations("acc").unwrap();
        assert_eq!(pending.len(), 1, "exactly one op is expected: {pending:?}");
        (
            pending[0].status.clone(),
            pending[0].last_error.clone().unwrap_or_default(),
        )
    }

    /// A server whose mailbox was rebuilt since the last sync: it reports a
    /// UIDVALIDITY the local cursor has never seen.
    fn rebuilt_server() -> RecordingProvider {
        RecordingProvider {
            select_uid_validity: Some(SEEDED_UID_VALIDITY + 41),
            ..Default::default()
        }
    }

    #[test]
    fn move_is_refused_when_the_source_uidvalidity_changed() {
        let db = temp_db();
        seed_message(&db); // acc:INBOX:7, cursor at SEEDED_UID_VALIDITY
        seed_mailbox(&db, "acc:Trash", "Trash", "trash");
        db.delete_message("acc:INBOX:7").unwrap();

        let mut p = rebuilt_server();
        let report = replay_with_provider(&db, "acc", &mut p).unwrap();

        assert!(
            p.move_calls.is_empty(),
            "UID 7 names another message now: nothing may be moved"
        );
        assert_eq!(report.failed, 1);
        let (status, error) = only_pending(&db);
        assert_eq!(status, STATUS_FAILED_PERMANENT, "never retried");
        assert!(
            error.contains("INBOX was rebuilt on the server") && error.contains("move a message"),
            "the operations list must say why: {error}"
        );
    }

    #[test]
    fn flag_read_and_keyword_on_a_live_row_are_refused_when_uidvalidity_changed() {
        type Queue = fn(&Database);
        let cases: [(&str, Queue); 3] = [
            ("flag", |db| db.set_flagged("acc:INBOX:7", true).unwrap()),
            ("mark_read", |db| db.mark_read("acc:INBOX:7", true).unwrap()),
            ("keyword", |db| {
                db.set_message_keyword("acc:INBOX:7", "Work", true).unwrap();
            }),
        ];
        for (kind, queue) in cases {
            let db = temp_db();
            seed_message(&db);
            queue(&db);

            let mut p = rebuilt_server();
            let report = replay_with_provider(&db, "acc", &mut p).unwrap();

            assert!(p.order.is_empty(), "{kind}: no STORE may reach the server");
            assert_eq!(report.failed, 1, "{kind}");
            let (status, error) = only_pending(&db);
            assert_eq!(status, STATUS_FAILED_PERMANENT, "{kind}");
            assert!(error.contains("rebuilt on the server"), "{kind}: {error}");
        }
    }

    #[test]
    fn state_op_stamped_by_a_move_carries_and_checks_the_uidvalidity() {
        let db = temp_db();
        seed_message(&db);
        seed_mailbox(&db, "acc:Trash", "Trash", "trash");
        db.mark_read("acc:INBOX:7", true).unwrap();
        db.delete_message("acc:INBOX:7").unwrap(); // stamps src_path/uid/uid_validity

        let pending = db.pending_operations("acc").unwrap();
        let stamped: serde_json::Value = serde_json::from_str(&pending[0].payload_json).unwrap();
        assert_eq!(pending[0].kind, "mark_read");
        assert_eq!(stamped["uid_validity"], SEEDED_UID_VALIDITY);

        // Sync has since recorded a NEW UID space for INBOX. The stamped op
        // must still be judged by the one its UID came from, not the cursor.
        seed_cursor(&db, "acc:INBOX", SEEDED_UID_VALIDITY + 41);
        let mut p = rebuilt_server();
        let report = replay_with_provider(&db, "acc", &mut p).unwrap();
        assert!(p.order.is_empty(), "neither the store nor the move may run");
        assert_eq!(report.failed, 2);
    }

    // ---- undo_move ----

    /// A `move` INBOX -> Archive that already `succeeded`, plus the
    /// `undo_move` queued for it. Returns the undo payload.
    fn seed_undo(db: &Database, message_id: &str) -> String {
        seed_message(db);
        seed_mailbox(db, "acc:Archive", "Archive", "archive");
        let out = db
            .move_messages(
                &["acc:INBOX:7".to_string()],
                castellum_db::MoveTarget::Archive,
            )
            .unwrap();
        let original = out.operation_ids[0];
        db.set_operation_status(original, "succeeded", None)
            .unwrap();
        db.undo_moves(&[original]).unwrap();
        let undo = db
            .pending_operations("acc")
            .unwrap()
            .into_iter()
            .find(|o| o.kind == "undo_move")
            .expect("undo op queued");
        let mut payload: serde_json::Value = serde_json::from_str(&undo.payload_json).unwrap();
        payload["message_id"] = message_id.into();
        payload.to_string()
    }

    #[test]
    fn replayed_move_records_where_it_landed() {
        let db = temp_db();
        seed_message(&db);
        seed_mailbox(&db, "acc:Archive", "Archive", "archive");
        let out = db
            .move_messages(
                &["acc:INBOX:7".to_string()],
                castellum_db::MoveTarget::Archive,
            )
            .unwrap();
        let op = db.pending_operations("acc").unwrap().remove(0);
        let mut p = RecordingProvider {
            reports_landing: Some(castellum_providers::MovedTo {
                uid_validity: 4242,
                uid: 77,
            }),
            ..Default::default()
        };
        apply_operation(&db, &mut p, "acc", op.id, 0, "move", &op.payload_json).unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(&db.operation_payload(out.operation_ids[0]).unwrap().unwrap())
                .unwrap();
        assert_eq!(payload["dest_uid"], 77);
        assert_eq!(payload["dest_uid_validity"], 4242);
    }

    #[test]
    fn undo_move_after_succeeded_move_moves_back_by_message_id() {
        let db = temp_db();
        let payload = seed_undo(&db, "<7@x>");
        let mut p = RecordingProvider::default();
        p.locate_returns.insert("Archive".into(), vec![33]);
        apply_operation(&db, &mut p, "acc", 99, 0, "undo_move", &payload).unwrap();
        assert_eq!(
            p.move_calls,
            vec![("Archive".into(), 33, "<7@x>".into(), "INBOX".into())]
        );
    }

    #[test]
    fn undo_move_with_several_exact_matches_fails_instead_of_guessing() {
        let db = temp_db();
        let payload = seed_undo(&db, "<7@x>");
        let mut p = RecordingProvider::default();
        p.locate_returns.insert("Archive".into(), vec![12, 40, 31]);
        let err = apply_operation(&db, &mut p, "acc", 99, 0, "undo_move", &payload)
            .expect_err("ambiguous duplicates must not be moved");
        assert!(
            matches!(err, CoreError::NonRetryable(ref m) if m.contains("Couldn't undo")),
            "{err:?}"
        );
        assert!(p.move_calls.is_empty());
    }

    /// Records the destination the way the replayed `move` does (COPYUID).
    fn record_landing(db: &Database, payload: &str, uid: i64) {
        let v: serde_json::Value = serde_json::from_str(payload).unwrap();
        db.record_move_result(v["undo_of"].as_i64().unwrap(), SEEDED_UID_VALIDITY, uid)
            .unwrap();
    }

    #[test]
    fn undo_move_prefers_recorded_dest_uid_over_any_search() {
        let db = temp_db();
        let payload = seed_undo(&db, "<7@x>");
        record_landing(&db, &payload, 55);
        let mut p = RecordingProvider::default();
        // A search would offer decoys / duplicates; it must not even be consulted.
        p.locate_returns.insert("Archive".into(), vec![12, 40]);
        apply_operation(&db, &mut p, "acc", 99, 0, "undo_move", &payload).unwrap();
        assert_eq!(
            p.move_calls,
            vec![("Archive".into(), 55, "<7@x>".into(), "INBOX".into())]
        );
    }

    #[test]
    fn undo_move_with_recorded_uid_works_without_a_message_id() {
        let db = temp_db();
        let payload = seed_undo(&db, "");
        record_landing(&db, &payload, 55);
        let mut p = RecordingProvider::default();
        apply_operation(&db, &mut p, "acc", 99, 0, "undo_move", &payload).unwrap();
        assert_eq!(p.move_calls[0].1, 55);
    }

    #[test]
    fn undo_move_refuses_when_the_destination_uidvalidity_changed() {
        let db = temp_db();
        let payload = seed_undo(&db, "<7@x>");
        record_landing(&db, &payload, 55);
        let mut p = RecordingProvider {
            select_uid_validity: Some(SEEDED_UID_VALIDITY + 9),
            ..Default::default()
        };
        let err = apply_operation(&db, &mut p, "acc", 99, 0, "undo_move", &payload)
            .expect_err("a rebuilt folder's UID 55 may be another message");
        assert!(matches!(err, CoreError::NonRetryable(_)), "{err:?}");
        assert!(p.move_calls.is_empty());
    }

    #[test]
    fn undo_move_not_found_is_non_retryable() {
        let db = temp_db();
        let payload = seed_undo(&db, "<7@x>");
        let mut p = RecordingProvider::default();
        let err = apply_operation(&db, &mut p, "acc", 99, 0, "undo_move", &payload)
            .expect_err("nothing to move back");
        assert!(matches!(err, CoreError::NonRetryable(_)), "{err:?}");
        assert!(p.move_calls.is_empty());
    }

    #[test]
    fn undo_move_already_back_in_destination_is_satisfied() {
        let db = temp_db();
        let payload = seed_undo(&db, "<7@x>");
        let mut p = RecordingProvider::default();
        p.locate_returns.insert("INBOX".into(), vec![7]);
        apply_operation(&db, &mut p, "acc", 99, 0, "undo_move", &payload).unwrap();
        assert!(p.move_calls.is_empty());
    }

    #[test]
    fn undo_move_without_message_id_is_non_retryable() {
        let db = temp_db();
        let payload = seed_undo(&db, "");
        let mut p = RecordingProvider::default();
        p.locate_returns.insert("Archive".into(), vec![33]);
        let err = apply_operation(&db, &mut p, "acc", 99, 0, "undo_move", &payload)
            .expect_err("no Message-ID, no recorded dest uid");
        assert!(matches!(err, CoreError::NonRetryable(_)), "{err:?}");
        assert!(p.move_calls.is_empty());
    }

    #[test]
    fn undo_move_waits_for_a_move_that_has_not_reached_the_server() {
        let db = temp_db();
        seed_message(&db);
        seed_mailbox(&db, "acc:Archive", "Archive", "archive");
        let out = db
            .move_messages(
                &["acc:INBOX:7".to_string()],
                castellum_db::MoveTarget::Archive,
            )
            .unwrap();
        db.undo_moves(&out.operation_ids).unwrap(); // original still queued
        let undo = db
            .pending_operations("acc")
            .unwrap()
            .into_iter()
            .find(|o| o.kind == "undo_move")
            .unwrap();
        let mut p = RecordingProvider::default();
        p.locate_returns.insert("Archive".into(), vec![33]);
        let err = apply_operation(
            &db,
            &mut p,
            "acc",
            undo.id,
            0,
            "undo_move",
            &undo.payload_json,
        )
        .expect_err("original not applied yet");
        assert!(matches!(err, CoreError::Sync(_)), "retryable: {err:?}");
        assert!(p.move_calls.is_empty());
    }

    #[test]
    fn undo_move_of_a_dead_move_is_a_no_op() {
        let db = temp_db();
        seed_message(&db);
        seed_mailbox(&db, "acc:Archive", "Archive", "archive");
        let out = db
            .move_messages(
                &["acc:INBOX:7".to_string()],
                castellum_db::MoveTarget::Archive,
            )
            .unwrap();
        db.set_operation_status(out.operation_ids[0], "failed_permanent", Some("x"))
            .unwrap();
        db.undo_moves(&out.operation_ids).unwrap();
        let undo = db
            .pending_operations("acc")
            .unwrap()
            .into_iter()
            .find(|o| o.kind == "undo_move")
            .unwrap();
        let mut p = RecordingProvider::default();
        apply_operation(
            &db,
            &mut p,
            "acc",
            undo.id,
            0,
            "undo_move",
            &undo.payload_json,
        )
        .unwrap();
        assert!(p.move_calls.is_empty());
    }

    #[test]
    fn move_payload_without_a_snapshot_falls_back_to_the_sync_cursor() {
        let db = temp_db();
        seed_message(&db);
        // An op queued by a build that did not read `uid_validity` back, and
        // one queued while the mailbox had no cursor (`0`).
        for payload in [
            serde_json::json!({"src_path": "INBOX", "uid": 7, "message_id": "<7@x>",
                               "dest_path": "Trash"}),
            serde_json::json!({"src_path": "INBOX", "uid": 7, "message_id": "<7@x>",
                               "dest_path": "Trash", "uid_validity": 0}),
        ] {
            let payload = payload.to_string();
            // Cursor and server agree: the move goes ahead.
            let mut same = RecordingProvider::default();
            apply_operation(&db, &mut same, "acc", 1, 0, "move", &payload).unwrap();
            assert_eq!(same.move_calls.len(), 1);

            // Server was rebuilt and sync has not noticed yet: refused.
            let mut rebuilt = rebuilt_server();
            let err = apply_operation(&db, &mut rebuilt, "acc", 1, 0, "move", &payload)
                .expect_err("a stale UID must not be moved");
            assert!(matches!(err, CoreError::NonRetryable(_)), "{err:?}");
            assert!(rebuilt.move_calls.is_empty());
        }
    }

    #[test]
    fn write_with_no_recorded_uidvalidity_waits_instead_of_guessing() {
        let db = temp_db();
        seed_message(&db);
        seed_mailbox(&db, "acc:Fresh", "Fresh", "custom"); // never synced: no cursor
        let payload = serde_json::json!({"src_path": "Fresh", "uid": 3, "message_id": "",
                                         "dest_path": "Trash", "uid_validity": 0})
        .to_string();

        let mut p = RecordingProvider::default();
        let err = apply_operation(&db, &mut p, "acc", 1, 0, "move", &payload)
            .expect_err("no baseline: nothing may be sent");
        assert!(p.move_calls.is_empty());
        assert!(
            matches!(err, CoreError::Sync(_)),
            "retryable, so it applies once a sync has recorded the UID space: {err:?}"
        );
    }

    #[test]
    fn server_that_reports_no_uidvalidity_is_refused() {
        let db = temp_db();
        seed_message(&db);
        db.set_flagged("acc:INBOX:7", true).unwrap();

        let mut p = RecordingProvider {
            select_uid_validity: Some(-1),
            ..Default::default()
        };
        replay_with_provider(&db, "acc", &mut p).unwrap();
        assert!(p.flag_calls.is_empty());
        assert_eq!(only_pending(&db).0, STATUS_FAILED_PERMANENT);
    }

    // ---: server_draft_cleanup checks UIDVALIDITY -------------

    fn seed_draft_account(db: &Database, account_id: &str) {
        db.upsert_account(&Account {
            id: account_id.into(),
            email: format!("{account_id}@x"),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.set_account_write_enabled(account_id, true).unwrap();
    }

    #[test]
    fn server_draft_cleanup_deletes_nothing_when_drafts_uidvalidity_changed() {
        let db = temp_db();
        seed_draft_account(&db, "acc");
        let draft_id = db.save_draft(&empty_draft("acc")).unwrap();
        db.record_server_draft_saved(draft_id, "acc", "Drafts", Some(1), Some(9), "<m1@x>")
            .unwrap();
        db.delete_draft(draft_id).unwrap(); // queues the cleanup of uid 9 in V1

        let mut provider = RecordingProvider {
            select_uid_validity: Some(2), // Drafts was rebuilt: uid 9 is someone else's
            ..Default::default()
        };
        let report = replay_with_provider(&db, "acc", &mut provider).unwrap();

        assert!(
            provider.delete_uid_calls.is_empty(),
            "uid 9 in the new UID space is a different message"
        );
        assert_eq!(report.failed, 0, "an orphan server draft is not a failure");
        assert!(db.pending_operations("acc").unwrap().is_empty());
    }

    #[test]
    fn server_draft_cleanup_without_a_recorded_uidvalidity_deletes_nothing() {
        let db = temp_db();
        seed_draft_account(&db, "acc");
        // Shapes an older build queued, and a mirror recorded with no value.
        for payload in [
            serde_json::json!({"mailbox_path": "Drafts", "uid": 9}),
            serde_json::json!({"mailbox_path": "Drafts", "uid": 9, "uid_validity": 0}),
            serde_json::json!({"mailbox_path": "Drafts", "uid": 9, "uid_validity": -1}),
        ] {
            let mut provider = RecordingProvider::default();
            apply_operation(
                &db,
                &mut provider,
                "acc",
                1,
                0,
                "server_draft_cleanup",
                &payload.to_string(),
            )
            .unwrap();
            assert!(provider.delete_uid_calls.is_empty(), "{payload}");
        }
    }

    // ---: a save queued before the From account changed -------

    #[test]
    fn stale_server_draft_save_is_not_mirrored_after_the_from_account_changes() {
        let db = temp_db();
        seed_draft_account(&db, "acc");
        seed_draft_account(&db, "acc2");
        let draft_id = db.save_draft(&empty_draft("acc")).unwrap();
        db.queue_server_draft_save(draft_id, "<m1@x>", b"raw")
            .unwrap();
        // The user switches From to acc2 before acc's replay ran (offline).
        let mut moved = empty_draft("acc2");
        moved.id = Some(draft_id);
        db.save_draft(&moved).unwrap();

        let mut provider = RecordingProvider {
            append_get_uid_returns: Some(AppendedUid {
                uid_validity: 1,
                uid: 42,
            }),
            ..Default::default()
        };
        replay_with_provider(&db, "acc", &mut provider).unwrap();

        assert!(
            provider.append_get_uid_calls.is_empty(),
            "stale content must not land in the old account's Drafts"
        );
        let draft = db.get_draft(draft_id).unwrap().unwrap();
        assert_eq!(draft.account_id, "acc2");
        assert_eq!(draft.server_uid, None, "no foreign UID on acc2's draft");
        assert_eq!(draft.server_mailbox_path, None);
        assert!(db.pending_operations("acc").unwrap().is_empty());
        assert!(db.pending_operations("acc2").unwrap().is_empty());
    }

    #[test]
    fn server_draft_save_arm_itself_refuses_a_draft_owned_by_another_account() {
        // The guard for an op `save_draft` could not cancel (it was already
        // `running`): the arm must compare the op's account with the draft's.
        let db = temp_db();
        seed_draft_account(&db, "acc");
        seed_draft_account(&db, "acc2");
        let draft_id = db.save_draft(&empty_draft("acc2")).unwrap();
        let payload = serde_json::json!({"draft_id": draft_id, "dest_path": "Drafts",
                                         "message_id": "<m1@x>"})
        .to_string();

        let mut provider = RecordingProvider::default();
        apply_operation(
            &db,
            &mut provider,
            "acc",
            1,
            0,
            "server_draft_save",
            &payload,
        )
        .unwrap();
        assert!(provider.append_get_uid_calls.is_empty());
        assert_eq!(db.get_draft(draft_id).unwrap().unwrap().server_uid, None);
    }

    // ---: a retried server_draft_save must not duplicate ------

    /// A Drafts mailbox that keeps what is appended to it. `lose_answers`
    /// makes that many APPENDs succeed on the server and then fail on the
    /// way back, which is how a retry comes to run after a stored copy.
    #[derive(Default)]
    struct DraftsServer {
        stored: Vec<i64>,
        next_uid: i64,
        lose_answers: usize,
        searches: usize,
        deleted: Vec<i64>,
    }

    impl MailProvider for DraftsServer {
        fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
            Ok(vec![])
        }
        fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
            Ok(MailboxState {
                uid_validity: 5,
                uid_next: self.next_uid + 1,
                exists: self.stored.len() as i64,
            })
        }
        fn fetch_new(&mut self, _after: Option<i64>) -> Result<Vec<RemoteMessage>, ProviderError> {
            Ok(vec![])
        }
        fn logout(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }
        fn append_with_flags_get_uid(
            &mut self,
            _mailbox_path: &str,
            _flags: &[MessageFlag],
            _raw: &[u8],
        ) -> Result<Option<AppendedUid>, ProviderError> {
            self.next_uid += 1;
            self.stored.push(self.next_uid);
            if self.lose_answers > 0 {
                self.lose_answers -= 1;
                return Err(ProviderError::Network("connection reset".into()));
            }
            Ok(Some(AppendedUid {
                uid_validity: 5,
                uid: self.next_uid,
            }))
        }
        fn find_uid_by_message_id(
            &mut self,
            _mailbox_path: &str,
            _message_id: &str,
        ) -> Result<Option<AppendedUid>, ProviderError> {
            unreachable!("the retry path asks for every copy, not just the newest")
        }
        fn find_uids_by_message_id(
            &mut self,
            _mailbox_path: &str,
            _message_id: &str,
        ) -> Result<MessageIdMatches, ProviderError> {
            self.searches += 1;
            Ok(MessageIdMatches {
                uid_validity: 5,
                uids: self.stored.clone(),
            })
        }
        fn delete_uid(&mut self, _mailbox_path: &str, uid: i64) -> Result<(), ProviderError> {
            self.stored.retain(|u| *u != uid);
            self.deleted.push(uid);
            Ok(())
        }
    }

    #[test]
    fn retried_server_draft_save_adopts_the_copy_the_lost_attempt_stored() {
        let db = temp_db();
        seed_draft_account(&db, "acc");
        let draft_id = db.save_draft(&empty_draft("acc")).unwrap();
        db.queue_server_draft_save(draft_id, "<m1@x>", b"raw")
            .unwrap();

        let mut server = DraftsServer {
            lose_answers: 1,
            ..Default::default()
        };
        let first = replay_with_provider(&db, "acc", &mut server).unwrap();
        assert_eq!(first.failed, 1, "the APPEND's answer was lost");
        assert_eq!(server.searches, 0, "a first attempt never pre-searches");
        assert_eq!(server.stored, vec![1], "but the copy is on the server");

        let second = replay_with_provider(&db, "acc", &mut server).unwrap();
        assert_eq!(second.applied, 1);
        assert_eq!(
            server.stored,
            vec![1],
            "the retry must not append a second copy"
        );
        let draft = db.get_draft(draft_id).unwrap().unwrap();
        assert_eq!(draft.server_uid, Some(1), "the stored copy is the mirror");
        assert_eq!(draft.server_uid_validity, Some(5));
    }

    #[test]
    fn retried_server_draft_save_cleans_up_every_extra_copy() {
        let db = temp_db();
        seed_draft_account(&db, "acc");
        let draft_id = db.save_draft(&empty_draft("acc")).unwrap();
        db.queue_server_draft_save(draft_id, "<m1@x>", b"raw")
            .unwrap();

        // Two copies already exist (two earlier attempts each lost their
        // answer, e.g. under a build without the pre-search).
        let mut server = DraftsServer {
            stored: vec![1, 2],
            next_uid: 2,
            lose_answers: 1,
            ..Default::default()
        };
        replay_with_provider(&db, "acc", &mut server).unwrap(); // stores 3, fails
        assert_eq!(server.stored, vec![1, 2, 3]);

        // The retry adopts the newest and queues the others; the cleanup ops
        // it queued run on the following pass.
        replay_with_provider(&db, "acc", &mut server).unwrap();
        assert_eq!(db.get_draft(draft_id).unwrap().unwrap().server_uid, Some(3));
        replay_with_provider(&db, "acc", &mut server).unwrap();
        assert_eq!(server.stored, vec![3], "only the mirror is left");
        assert_eq!(server.deleted, vec![1, 2]);
        assert!(db.pending_operations("acc").unwrap().is_empty());
    }
}
