// Castellum - outbound send executor (SMTP delivery of queued drafts).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Drains `send` operations for an account through the same status/attempts/
// backoff machinery as the IMAP replayer (`crate::replay`), but delivers via
// an `SmtpSender` instead of a `MailProvider`. On success the op succeeds, the
// draft becomes `sent`, and a durable `sent_copy` op is enqueued for the IMAP
// replayer (servers don't file SMTP submissions into Sent); on failure the op
// is marked failed (retried later with backoff) and the draft stays `outbox`.

use crate::redact;
use crate::replay::{
    backoff_seconds, db_err, due_for_attempt, failure_status, now_epoch, retire_exhausted_ops,
    STATUS_FAILED, STATUS_FAILED_PERMANENT,
};
use crate::CoreError;
use castellum_db::Database;
use castellum_models::{OperationRecord, OutboxSend};
use castellum_provider_smtp::{build_rfc822_opts, BuildOptions, LettreSmtpSender};
use castellum_providers::{ProviderError, SmtpConfig, SmtpSender};
use serde::{Deserialize, Serialize};

/// Operation kind for queued outbound sends. The IMAP replayer skips this kind;
/// only this executor drains it.
pub(crate) const SEND_KIND: &str = "send";

/// Send request (deserialized from the FFI JSON): who is sending, and the SMTP
/// endpoint to submit through. Credentials are passed per call, like sync/replay.
#[derive(Debug, Deserialize)]
pub struct SendConfig {
    pub account_id: String,
    pub from_email: String,
    #[serde(default)]
    pub from_name: Option<String>,
    /// MM-17 S2: send plain-text-only drafts as RFC 3676 `format=flowed`
    /// (Settings > General). Absent = false = today's bytes exactly.
    #[serde(default)]
    pub plain_text_flowed: bool,
    pub smtp: SmtpSettings,
}

/// SMTP connection parameters (JSON shape of [`SmtpConfig`]).
///
/// `Debug` is implemented manually to redact `password` — a stray `{:?}` of a
/// config (e.g. in an error message) must never leak a credential. `SendConfig`
/// keeps its derived `Debug`, which nests this redacting one.
#[derive(Deserialize)]
pub struct SmtpSettings {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    #[serde(default = "default_true")]
    pub use_tls: bool,
    #[serde(default)]
    pub allow_invalid_certs: bool,
}

impl std::fmt::Debug for SmtpSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpSettings")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("use_tls", &self.use_tls)
            .field("allow_invalid_certs", &self.allow_invalid_certs)
            .finish()
    }
}

fn default_true() -> bool {
    true
}

/// Kind + detail for a `ProviderError`, without going through `redact`'s
/// fixed IMAP-flavoured summaries (SMTP needs its own wording — see
/// `AccountSyncStatus`'s Swift-side counterpart, `SendFailurePresentation`,
/// in `apple/Sources/Castellum/App/AccountSyncStatus.swift`). The detail is
/// kept — unlike the durable sync diagnostics, `pending_operations.last_error`
/// is local-only (already shown verbatim in the Outbox for every op kind) —
/// callers that build a user-facing message read only the leading kind token
/// and discard the detail, same discipline `AccountSyncStatus.message` uses.
fn provider_kind_and_detail(e: &ProviderError) -> (redact::ErrorKind, &str) {
    match e {
        ProviderError::Auth(m) => (redact::ErrorKind::Auth, m.as_str()),
        ProviderError::AuthSmtpDisabled(m) => (redact::ErrorKind::AuthSmtpDisabled, m.as_str()),
        ProviderError::Tls(m) => (redact::ErrorKind::Tls, m.as_str()),
        ProviderError::Network(m) => (redact::ErrorKind::Network, m.as_str()),
        ProviderError::Protocol(m) => (redact::ErrorKind::Protocol, m.as_str()),
        ProviderError::Unsupported(m) => (redact::ErrorKind::Unsupported, m.as_str()),
        ProviderError::Rejected(m) => (redact::ErrorKind::Rejected, m.as_str()),
        ProviderError::Deferred(m) => (redact::ErrorKind::Deferred, m.as_str()),
        ProviderError::MaybeDelivered(m) => (redact::ErrorKind::Network, m.as_str()),
    }
}

/// Renders `"<kind>: <detail>"` — the stable, parseable prefix the Swift side
/// reads (only the kind token; detail is for local debugging/Outbox display).
pub(crate) fn classified_provider_error(e: &ProviderError) -> String {
    let (kind, detail) = provider_kind_and_detail(e);
    format!("{}: {detail}", kind.as_str())
}

/// A failed send attempt: `Provider` failures (building or submitting the
/// message) carry the original `ProviderError` so the caller can classify
/// them by kind; everything else (bad payload, db, an already-final draft)
/// stays a plain `CoreError` with its existing generic formatting.
enum ApplySendError {
    Provider(ProviderError),
    Other(CoreError),
    /// The submission may already have reached the server, so retrying could
    /// mail the recipients a second copy: the op is held, never retried
    /// automatically.
    MaybeSent,
}

/// `last_error` of a send held because its submission may already have
/// reached the server. The "may have been sent" prefix is the contract with
/// the FFI header and the Swift Outbox (see `reconcile_undo_sends`).
const MAYBE_SENT_ERROR: &str =
    "may have been sent: This message may have been sent. Check Sent before retrying.";

impl From<CoreError> for ApplySendError {
    fn from(e: CoreError) -> Self {
        ApplySendError::Other(e)
    }
}

/// Outcome of a send run.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct SendReport {
    pub account_id: String,
    /// Drafts delivered successfully this run.
    pub sent: usize,
    /// Send ops that failed (left as `failed`, with the error recorded).
    pub failed: usize,
    /// Queued `send` ops NOT attempted because the account is in evaluation
    /// (read-only) mode. They stay queued and visible; nothing is delivered.
    #[serde(default)]
    pub blocked: usize,
}

/// Payload for a `send` operation.
#[derive(Debug, Deserialize)]
struct SendPayload {
    draft_id: i64,
}

/// Parse the JSON config, build a blocking SMTP sender, drain the queue, and
/// return a JSON report.
pub fn send_pending_json(db: &Database, config_json: &str) -> Result<String, CoreError> {
    let cfg: SendConfig = serde_json::from_str(config_json)
        .map_err(|e| CoreError::Sync(format!("bad config json: {e}")))?;

    // Evaluation mode: do not build an SMTP connection — return blocked report.
    if !db.account_write_enabled(&cfg.account_id).map_err(db_err)? {
        let pending = db.pending_operations(&cfg.account_id).map_err(db_err)?;
        let blocked = pending.iter().filter(|op| op.kind == SEND_KIND).count();
        let report = SendReport {
            account_id: cfg.account_id.clone(),
            sent: 0,
            failed: 0,
            blocked,
        };
        return serde_json::to_string(&report)
            .map_err(|e| CoreError::Sync(format!("serialize report: {e}")));
    }

    let mut sender = LettreSmtpSender::new(&SmtpConfig {
        host: cfg.smtp.host.clone(),
        port: cfg.smtp.port,
        username: cfg.smtp.username.clone(),
        password: cfg.smtp.password.clone(),
        use_tls: cfg.smtp.use_tls,
        allow_invalid_certs: cfg.smtp.allow_invalid_certs,
    })
    // Connecting fails before any op is even claimed, so there is no
    // `pending_operations` row to carry the classification — it must travel
    // in the thrown `CoreError` itself (Swift reads it via
    // `CastellumError.message`; see `classified_provider_error`).
    .map_err(|e| CoreError::Sync(classified_provider_error(&e)))?;

    let report = send_pending_with_sender_opts(
        db,
        &cfg.account_id,
        &cfg.from_email,
        cfg.from_name.as_deref(),
        cfg.plain_text_flowed,
        &mut sender,
        now_epoch(),
    )?;

    serde_json::to_string(&report).map_err(|e| CoreError::Sync(format!("serialize report: {e}")))
}

/// Core send algorithm, sender-agnostic (so tests can drive a mock).
///
/// Attempts each *due* `send` op via the sender, oldest first. A build/submit
/// failure marks that op `failed` (recording the error) and continues; failed
/// ops retry on later runs with the shared backoff, up to `MAX_ATTEMPTS`. Only
/// a database error aborts the run.
pub fn send_pending_with_sender(
    db: &Database,
    account_id: &str,
    from_email: &str,
    from_name: Option<&str>,
    sender: &mut dyn SmtpSender,
) -> Result<SendReport, CoreError> {
    send_pending_with_sender_at(db, account_id, from_email, from_name, sender, now_epoch())
}

/// `send_pending_with_sender` with an injected clock (`now`, epoch seconds):
/// a scheduled send (Send Later, HP1) is attempted only once `now` has reached
/// its `send_at`, and not at all once it is more than the late-send grace past
/// it (it becomes `needs_confirmation` instead).
pub fn send_pending_with_sender_at(
    db: &Database,
    account_id: &str,
    from_email: &str,
    from_name: Option<&str>,
    sender: &mut dyn SmtpSender,
    now: i64,
) -> Result<SendReport, CoreError> {
    send_pending_with_sender_opts(db, account_id, from_email, from_name, false, sender, now)
}

/// `send_pending_with_sender_at` plus the MM-17 `plain_text_flowed` switch.
/// Every shorter-named variant is this with the switch off.
#[allow(clippy::too_many_arguments)]
pub fn send_pending_with_sender_opts(
    db: &Database,
    account_id: &str,
    from_email: &str,
    from_name: Option<&str>,
    plain_text_flowed: bool,
    sender: &mut dyn SmtpSender,
    now: i64,
) -> Result<SendReport, CoreError> {
    // Undo-send crash-safety hold (S25), run from THIS drain path too — not
    // only from Swift's launch-time reconcile (`AppModel.
    // reconcileUndoSendsThenReplay`) — to close a launch race: that reconcile
    // runs off-actor and asynchronously, so an immediate Send triggered right
    // after relaunch can reach `sendPending` before it has completed, and
    // would otherwise claim an interrupted delayed send before it was ever
    // held. `Database::reconcile_undo_sends` is gated to run its actual hold
    // pass at MOST ONCE per account per launch (per open `Database`), so
    // calling it here on every drain is a no-op once that one execution —
    // whichever caller reaches it first, the launch task or this call itself
    // — has happened; it can never re-hold a legitimate in-session undo
    // window queued after that point. It MUST run before
    // `touch_last_clean_drain` below: the hold pass judges the heartbeat as
    // it stood BEFORE this drain, and touching it first would let this very
    // call's own claim escape the hold it exists to enforce.
    //
    // Corollary for callers (tests included): a `Database` that has NEVER had
    // `reconcile_undo_sends` run for an account is, by construction,
    // indistinguishable here from "just relaunched, possibly with a crash
    // survivor" — so the first due delayed send ever drained for it WILL be
    // held. Real usage never hits this: `AppModel.bootstrap()` reconciles
    // every account before compose is even reachable. A test that wants to
    // exercise ordinary (non-crash) delayed-send delivery must call
    // `db.reconcile_undo_sends(account_id)` once, before queuing, to
    // represent that launch-time sweep having already run clean — see
    // `skip_before_time_a_delayed_send_is_not_delivered_inside_its_window`
    // and `delivers_once_the_undo_window_ends` below; a test that wants to
    // prove the race fix itself instead skips that call on purpose — see
    // `send_drain_holds_an_interrupted_delayed_send_even_without_an_explicit_reconcile_first`.
    // See DECISIONS.md.
    db.reconcile_undo_sends_at(account_id, now)
        .map_err(db_err)?;

    // Undo-send crash-safety heartbeat (S25): touched at the START of every
    // drain call, whatever it goes on to find, so it only ever advances while
    // the app is actually alive and calling this. `reconcile_undo_sends`
    // (above, and at the next launch) reads it back to decide whether a
    // still-queued delayed send's undo window can be trusted to have stayed
    // open. See DECISIONS.md.
    db.touch_last_clean_drain(now).map_err(db_err)?;

    let pending = db.pending_operations(account_id).map_err(db_err)?;

    // Evaluation mode (fail-closed): deliver nothing. Queued `send` ops stay
    // visible in the outbox with no attempt consumed. Enforcement authority for
    // SMTP — outbox retries and restart recovery route here too.
    if !db.account_write_enabled(account_id).map_err(db_err)? {
        let blocked = pending.iter().filter(|op| op.kind == SEND_KIND).count();
        return Ok(SendReport {
            account_id: account_id.to_string(),
            sent: 0,
            failed: 0,
            blocked,
        });
    }

    // Retire exhausted `send` ops to the terminal `failed_permanent` (crash at the
    // ceiling, or already failed at the cap), same as the replayer does for its own
    // ops. `for_send = true` restricts it to the ops we own.
    retire_exhausted_ops(db, &pending, true)?;

    let mut sent = 0usize;
    let mut failed = 0usize;
    let mut blocked = 0usize;

    for op in pending {
        if op.kind != SEND_KIND || !due_for_attempt(&op, now) {
            continue;
        }
        // Live kill-switch: re-read the policy per op so disabling writes
        // mid-batch stops the remaining sends. Leave the op queued; no attempt.
        if !db.account_write_enabled(account_id).map_err(db_err)? {
            blocked += 1;
            continue;
        }
        match attempt_send_op(
            db,
            account_id,
            from_email,
            from_name,
            plain_text_flowed,
            sender,
            &op,
        )? {
            Some(true) => sent += 1,
            Some(false) => failed += 1,
            None => {}
        }
    }

    Ok(SendReport {
        account_id: account_id.to_string(),
        sent,
        failed,
        blocked,
    })
}

/// The status a failed send attempt is recorded with. An authentication
/// refusal is permanent at once (OB1): repeating it cannot succeed until the
/// user fixes the sign-in, and hammering a server with a refused password can
/// lock the account. Everything else keeps the shared attempt budget
/// (`failure_status`). A permanent failure is never retried automatically
/// (`due_for_attempt`), only by Send Now / Retry / Send All.
fn send_failure_status(e: &ApplySendError, attempts_after: i64) -> &'static str {
    match e {
        // Never double-send: only the user's Send Now / Back to Drafts moves
        // a held op on.
        ApplySendError::MaybeSent | ApplySendError::Provider(ProviderError::MaybeDelivered(_)) => {
            "held"
        }
        ApplySendError::Provider(ProviderError::Auth(_)) => STATUS_FAILED_PERMANENT,
        // SMTP AUTH switched off (MS1) is a mailbox or tenant setting: every
        // later attempt would be refused the same way, so it is terminal at
        // once instead of burning the backoff budget. The op stays visible in
        // the Outbox, where the manual Retry requeues it after the change.
        ApplySendError::Provider(ProviderError::AuthSmtpDisabled(_)) => STATUS_FAILED_PERMANENT,
        // A permanent 5xx refusal of the message (G8, #3632): the same bytes
        // get the same answer, so retrying for hours only delays telling the
        // person. Manual Retry in the Outbox still requeues it.
        ApplySendError::Provider(ProviderError::Rejected(_)) => STATUS_FAILED_PERMANENT,
        _ => failure_status(attempts_after),
    }
}

/// The log line for a failed send attempt, chosen by the status it was
/// recorded with. A held send may already be in the recipients' inboxes, so
/// it must not be logged as "delivery failed": someone reading the log would
/// resend it by hand and mail a second copy.
fn send_failure_log_message(status: &str) -> &'static str {
    match status {
        "held" => "send: held, the message may have been delivered",
        _ => "send: delivery failed",
    }
}

/// Claim one `send` op and try to deliver it. `Some(true)` = delivered,
/// `Some(false)` = failed (status and classified error recorded on the op),
/// `None` = the claim was refused, so nothing was attempted. Shared by the
/// automatic drain and by Send Now so both record outcomes identically.
fn attempt_send_op(
    db: &Database,
    account_id: &str,
    from_email: &str,
    from_name: Option<&str>,
    plain_text_flowed: bool,
    sender: &mut dyn SmtpSender,
    op: &OperationRecord,
) -> Result<Option<bool>, CoreError> {
    // A refused claim = the op went terminal (e.g. the user cancelled the
    // send from the outbox) after our pending snapshot — the message must
    // NOT be delivered. It is a compare-and-swap on the snapshot's status
    // (OB1): if a concurrent Send Now or drain already claimed it, skip it
    // rather than submit the same message twice.
    if !db
        .begin_operation_attempt_from(op.id, &op.status)
        .map_err(db_err)?
    {
        return Ok(None);
    }

    let delivered = match apply_send(
        db,
        from_email,
        from_name,
        plain_text_flowed,
        sender,
        &op.payload_json,
    ) {
        Ok(sent_message_id) => {
            db.set_operation_status(op.id, "succeeded", None)
                .map_err(db_err)?;
            // AW2: PROPOSE a waiting-for-reply item for what just left.
            // Best-effort and idempotent per draft: a failure here must
            // never fail a delivered send, and the user decides whether
            // the proposal ever becomes a work item.
            if let Ok(p) = serde_json::from_str::<SendPayload>(&op.payload_json) {
                let _ = crate::suggestions::propose_after_send_now(
                    db,
                    p.draft_id,
                    account_id,
                    from_email,
                    sent_message_id.as_deref(),
                );
            }
            true
        }
        Err(e) => {
            let status = send_failure_status(&e, op.attempts + 1);
            let recorded = match &e {
                // Classified: "<kind>: <detail>" — see
                // `classified_provider_error`.
                ApplySendError::MaybeSent
                | ApplySendError::Provider(ProviderError::MaybeDelivered(_)) => {
                    MAYBE_SENT_ERROR.to_string()
                }
                ApplySendError::Provider(pe) => classified_provider_error(pe),
                ApplySendError::Other(ce) => format!("{ce:?}"),
            };
            // Structured kind only in the log (never the detail text —
            // unlike the Outbox's local-only display, a log line
            // shouldn't carry it; see `provider_kind_and_detail`'s doc
            // comment for why the detail is otherwise trusted locally).
            let error_kind = match &e {
                ApplySendError::Provider(pe) => provider_kind_and_detail(pe).0.as_str(),
                ApplySendError::Other(_) | ApplySendError::MaybeSent => "internal",
            };
            let message = send_failure_log_message(status);
            tracing::warn!(account_id, error_kind, status, "{message}");
            db.set_operation_status(op.id, status, Some(&recorded))
                .map_err(db_err)?;
            false
        }
    };
    Ok(Some(delivered))
}

/// The draft a `send` op delivers, read from its payload.
fn send_op_draft_id(op: &OperationRecord) -> Option<i64> {
    serde_json::from_str::<SendPayload>(&op.payload_json)
        .ok()
        .map(|p| p.draft_id)
}

/// The closed set of failure kinds the Outbox may show. Anything else that
/// ended up in `last_error` (a database error, an old free-text row) collapses
/// to `internal`, so no server text can ever reach the list.
fn outbox_error_kind(last_error: &str) -> &'static str {
    match last_error.split_once(": ").map(|(kind, _)| kind) {
        Some("auth") => "auth",
        Some("tls") => "tls",
        Some("network") => "network",
        Some("protocol") => "protocol",
        Some("unsupported") => "unsupported",
        // A 4xx: the Outbox says "it will retry automatically" (the Swift
        // default), not "couldn't prepare this message" (`internal`).
        Some("deferred") => "deferred",
        _ => "internal",
    }
}

/// Every message waiting in the Outbox for `account_id` (OB1): each draft in
/// `outbox` status with the state of its live `send` op. Driven from the
/// DRAFTS, not the op queue, so a message whose op was cancelled or lost is
/// still listed (as `stranded`) instead of silently sitting unsent. `now`
/// (epoch seconds) is only used to report when the next automatic attempt is
/// due. Oldest first.
pub fn outbox_sends(
    db: &Database,
    account_id: &str,
    now: i64,
) -> Result<Vec<OutboxSend>, CoreError> {
    let pending = db.pending_operations(account_id).map_err(db_err)?;
    let mut ops: std::collections::HashMap<i64, &OperationRecord> =
        std::collections::HashMap::new();
    for op in pending.iter().filter(|op| op.kind == SEND_KIND) {
        if let Some(draft_id) = send_op_draft_id(op) {
            ops.entry(draft_id).or_insert(op);
        }
    }
    let mut rows: Vec<OutboxSend> = db
        .list_drafts(account_id)
        .map_err(db_err)?
        .into_iter()
        .filter(|d| d.status == "outbox")
        .map(|d| {
            let op = ops.get(&d.id);
            let status = op.map_or("stranded", |op| op.status.as_str());
            let failed = status == STATUS_FAILED || status == STATUS_FAILED_PERMANENT;
            let next_attempt_epoch = op.and_then(|op| match status {
                "queued" => op.not_before_epoch.filter(|at| *at > now),
                STATUS_FAILED => Some(
                    (op.updated_at_epoch + backoff_seconds(op.attempts))
                        .max(op.not_before_epoch.unwrap_or(0)),
                ),
                _ => None,
            });
            OutboxSend {
                draft_id: d.id,
                account_id: d.account_id,
                operation_id: op.map(|op| op.id),
                subject: d.subject,
                recipients: d
                    .to
                    .iter()
                    .chain(d.cc.iter())
                    .map(|a| a.email.clone())
                    .collect(),
                status: status.to_string(),
                attempts: op.map_or(0, |op| op.attempts),
                error_kind: op
                    .filter(|_| failed)
                    .and_then(|op| op.last_error.as_deref())
                    .map(|e| outbox_error_kind(e).to_string()),
                automatic_retry: matches!(status, "queued" | "running" | STATUS_FAILED),
                next_attempt_epoch,
                scheduled: op.is_some_and(|op| op.scheduled),
                updated_at_epoch: op.map_or(d.updated_at_epoch, |op| op.updated_at_epoch),
            }
        })
        .collect();
    rows.sort_by_key(|r| r.draft_id);
    Ok(rows)
}

/// Outbox "Send Now" (OB1): attempt exactly the message `draft_id`, right
/// now, whatever state its send is in. It is the user's explicit choice, so it
/// ignores the backoff and the attempt cap, releases a held / needs-
/// confirmation / scheduled send, gives a failed or permanently failed one a
/// fresh budget and re-queues a stranded one. No other message is touched.
///
/// Still refused, with nothing attempted: a read-only account
/// (`blocked = 1`), and a send that is in flight right now (`running`: a
/// second submission would double-send; the automatic drain owns it).
pub fn send_draft_now_with_sender_at(
    db: &Database,
    account_id: &str,
    draft_id: i64,
    from_email: &str,
    from_name: Option<&str>,
    sender: &mut dyn SmtpSender,
    now: i64,
) -> Result<SendReport, CoreError> {
    send_draft_now_opts(
        db, account_id, draft_id, from_email, from_name, false, sender, now,
    )
}

/// `send_draft_now_with_sender_at` plus the MM-17 `plain_text_flowed` switch.
#[allow(clippy::too_many_arguments)]
pub fn send_draft_now_opts(
    db: &Database,
    account_id: &str,
    draft_id: i64,
    from_email: &str,
    from_name: Option<&str>,
    plain_text_flowed: bool,
    sender: &mut dyn SmtpSender,
    now: i64,
) -> Result<SendReport, CoreError> {
    // Same crash-safety order as the drain: hold pass first, then heartbeat.
    db.reconcile_undo_sends_at(account_id, now)
        .map_err(db_err)?;
    db.touch_last_clean_drain(now).map_err(db_err)?;

    let mut report = SendReport {
        account_id: account_id.to_string(),
        sent: 0,
        failed: 0,
        blocked: 0,
    };
    let in_outbox = db
        .get_draft(draft_id)
        .map_err(db_err)?
        .is_some_and(|d| d.account_id == account_id && d.status == "outbox");
    if !in_outbox {
        return Ok(report);
    }
    if !db.account_write_enabled(account_id).map_err(db_err)? {
        report.blocked = 1;
        return Ok(report);
    }

    let find = |db: &Database| -> Result<Option<OperationRecord>, CoreError> {
        Ok(db
            .pending_operations(account_id)
            .map_err(db_err)?
            .into_iter()
            .find(|op| op.kind == SEND_KIND && send_op_draft_id(op) == Some(draft_id)))
    };
    match find(db)? {
        Some(op) if op.status == "running" => return Ok(report),
        Some(op) => {
            // Release whatever was keeping it from being due.
            db.send_now(draft_id).map_err(db_err)?;
            db.retry_operation(op.id).map_err(db_err)?;
        }
        None => {
            db.requeue_stranded_send(draft_id).map_err(db_err)?;
        }
    }
    // Re-read after the release so the claim compares against the status the
    // op has NOW; a drain that grabbed it meanwhile shows up as `running`.
    let op = match find(db)? {
        Some(op) if op.status != "running" => op,
        _ => return Ok(report),
    };
    match attempt_send_op(
        db,
        account_id,
        from_email,
        from_name,
        plain_text_flowed,
        sender,
        &op,
    )? {
        Some(true) => report.sent = 1,
        Some(false) => report.failed = 1,
        None => {}
    }
    Ok(report)
}

/// `send_draft_now_with_sender_at` over a real SMTP connection built from
/// `config_json` (the same `SendConfig` `send_pending_json` takes). Returns a
/// JSON `SendReport` for that one message.
pub fn send_draft_now_json(
    db: &Database,
    config_json: &str,
    draft_id: i64,
) -> Result<String, CoreError> {
    let cfg: SendConfig = serde_json::from_str(config_json)
        .map_err(|e| CoreError::Sync(format!("bad config json: {e}")))?;
    let report = if db.account_write_enabled(&cfg.account_id).map_err(db_err)? {
        let mut sender = LettreSmtpSender::new(&SmtpConfig {
            host: cfg.smtp.host.clone(),
            port: cfg.smtp.port,
            username: cfg.smtp.username.clone(),
            password: cfg.smtp.password.clone(),
            use_tls: cfg.smtp.use_tls,
            allow_invalid_certs: cfg.smtp.allow_invalid_certs,
        })
        .map_err(|e| CoreError::Sync(classified_provider_error(&e)))?;
        send_draft_now_opts(
            db,
            &cfg.account_id,
            draft_id,
            &cfg.from_email,
            cfg.from_name.as_deref(),
            cfg.plain_text_flowed,
            &mut sender,
            now_epoch(),
        )?
    } else {
        // Evaluation mode: never build an SMTP connection.
        let in_outbox = db
            .get_draft(draft_id)
            .map_err(db_err)?
            .is_some_and(|d| d.account_id == cfg.account_id && d.status == "outbox");
        SendReport {
            account_id: cfg.account_id.clone(),
            sent: 0,
            failed: 0,
            blocked: usize::from(in_outbox),
        }
    };
    serde_json::to_string(&report).map_err(|e| CoreError::Sync(format!("serialize report: {e}")))
}

/// Deliver one queued draft. Errors here are per-op (recorded as `failed`).
/// Build/submit failures stay `ApplySendError::Provider` so the caller can
/// classify them by kind (auth/tls/network/protocol/unsupported); every other
/// failure (bad payload, db) is `Other`, formatted generically as before.
fn apply_send(
    db: &Database,
    from_email: &str,
    from_name: Option<&str>,
    plain_text_flowed: bool,
    sender: &mut dyn SmtpSender,
    payload_json: &str,
) -> Result<Option<String>, ApplySendError> {
    let p: SendPayload = serde_json::from_str(payload_json)
        .map_err(|e| CoreError::Sync(format!("bad send payload: {e}")))?;
    let Some(draft) = db.get_draft(p.draft_id).map_err(db_err)? else {
        // The draft row is gone — nothing can be delivered. Succeed so the op
        // clears instead of sticking failed in the outbox forever.
        return Ok(None);
    };
    if draft.status == "sent" {
        // Already delivered (a crash landed between the SMTP submit and the op
        // status update). Do NOT submit again — that would double-send.
        return Ok(None);
    }

    // Outgoing attachments (S33): metadata + bytes, fetched here (not at
    // queue time) so listing a draft never loads payloads.
    let attachment_metas = db.list_draft_attachments(draft.id).map_err(db_err)?;
    let mut attachments = Vec::with_capacity(attachment_metas.len());
    for meta in attachment_metas {
        let bytes = db
            .draft_attachment_bytes(meta.id)
            .map_err(db_err)?
            .ok_or_else(|| CoreError::Sync(format!("attachment {} missing its bytes", meta.id)))?;
        attachments.push((meta, bytes));
    }

    // S47: an identity/alias chosen in the compose From picker is
    // persisted directly on the draft (Draft.from_email/from_name,
    // migration 0026) so it survives long after the compose window
    // closed — a delayed undo-send drain or a crash-recovered outbox
    // retry must still honour it, even though `from_email`/`from_name`
    // here (from `SendConfig`) are the ACCOUNT's own address and apply
    // uniformly to every draft this drain pass delivers. Falls back to
    // that account address when the draft never chose one — today's
    // behaviour, unchanged.
    let effective_from_email = draft.from_email.as_deref().unwrap_or(from_email);
    let effective_from_name = draft.from_name.as_deref().or(from_name);
    // MM-6: the sending identity's Reply-To header and automatic Bcc are read
    // here, at delivery time, from the identity that matches the address the
    // draft is actually sent from — so they hold for a delayed undo-send drain
    // and a crash-recovered retry exactly like the From address itself. The
    // account's own address is not an identity row and carries neither.
    let identity = db
        .list_identities(&draft.account_id)
        .map_err(db_err)?
        .into_iter()
        .find(|i| i.email.eq_ignore_ascii_case(effective_from_email));
    let mut msg = build_rfc822_opts(
        effective_from_email,
        effective_from_name,
        BuildOptions {
            reply_to: identity.as_ref().and_then(|i| i.reply_to.as_deref()),
            plain_text_flowed,
        },
        &draft,
        &attachments,
    )
    .map_err(ApplySendError::Provider)?;
    if let Some(bcc) = identity.as_ref().and_then(|i| i.bcc.as_deref()) {
        // Envelope only: Bcc never appears in the headers. Skip when the
        // address is already a recipient.
        if !msg.rcpts.iter().any(|r| r.eq_ignore_ascii_case(bcc)) {
            msg.rcpts.push(bcc.to_string());
        }
    }
    sender
        .send(&msg.from, &msg.rcpts, &msg.raw)
        .map_err(ApplySendError::Provider)?;
    // At-most-once plus a prompt: a crash or quit between the
    // SMTP submission above and the DB write below leaves the draft "outbox"
    // and the op `running`. Whether the server accepted the message cannot be
    // known from here, so the next launch does NOT re-submit it: the launch
    // pass (`Database::reconcile_undo_sends`) turns every send it finds
    // `running` into `held` with a "may have been sent" error, and only the
    // user's Send Now / Back to Drafts moves it on. (This used to be an
    // accepted at-least-once double-send.) The narrower window — a crash AFTER
    // mark_draft_sent_with_copy, before the op flips to succeeded — is closed
    // by the `status == "sent"` guard above. When the process survives but
    // that write FAILS (disk full, I/O error) the server has already said
    // 250 and the draft is still "outbox": the op is held (`MaybeSent`), not
    // given the retry budget, or the next drain would submit it again.
    //
    // Real IMAP servers do not file SMTP submissions into Sent, so alongside
    // marking the draft sent we enqueue (same transaction) a durable `sent_copy`
    // op carrying the delivered bytes; the IMAP replayer APPENDs it to the Sent
    // mailbox. Keeping it a separate queued op means a failure to copy is NEVER
    // a send failure — the mail is already out; the copy retries independently
    // and any terminal failure is recorded on that op, visible in the outbox.
    db.mark_draft_sent_with_copy(draft.id, &msg.message_id, &msg.raw)
        .map_err(|_| ApplySendError::MaybeSent)?;
    Ok(Some(msg.message_id.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::{replay_with_provider, MAX_ATTEMPTS};
    use castellum_models::{Account, DraftAddress, NewDraft};
    use castellum_providers::{
        MailProvider, MailboxState, ProviderError, RemoteMailbox, RemoteMessage,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_db() -> Database {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-send-test-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        Database::open(&path).expect("open db")
    }

    /// The Swift side stores accounts FLAT (`email`, `host`, `smtp_host`, …);
    /// `send_pending_json` wants a `SendConfig` (`from_email` plus a nested
    /// `smtp` object). Handing it the stored shape must fail loudly here, not
    /// look like "nothing to send" — `AppModel.drainSendPending` did exactly
    /// that, and its `try?` swallowed the error, so an undo-send window that
    /// simply ran out never delivered. Pin the rejection so the two shapes can
    /// never be quietly confused again.
    #[test]
    fn send_pending_rejects_the_flat_stored_account_config_shape() {
        let db = temp_db();
        let stored = r#"{"account_id":"acc","email":"alice@example.com","host":"127.0.0.1",
            "port":3143,"username":"alice@example.com","password":"pw","use_tls":false,
            "smtp_host":"127.0.0.1","smtp_port":3025}"#;
        let err = send_pending_json(&db, stored).expect_err("flat config must be rejected");
        assert!(
            format!("{err:?}").contains("bad config json"),
            "expected a config-shape error, got {err:?}"
        );
    }

    fn seed_account(db: &Database) {
        db.upsert_account(&Account {
            id: "acc".into(),
            email: "alice@example.com".into(),
            display_name: Some("Alice".into()),
            provider: "imap".into(),
        })
        .unwrap();
        // These tests exercise SMTP delivery, so leave evaluation mode.
        db.set_account_write_enabled("acc", true).unwrap();
    }

    fn queue_draft(db: &Database, subject: &str) -> i64 {
        let id = db
            .save_draft(&NewDraft {
                id: None,
                account_id: "acc".into(),
                to: vec![DraftAddress {
                    name: None,
                    email: "bob@example.com".into(),
                }],
                cc: vec![],
                bcc: vec![DraftAddress {
                    name: None,
                    email: "hidden@example.com".into(),
                }],
                subject: subject.into(),
                body_text: "body\n".into(),
                body_html: None,
                from_email: None,
                from_name: None,
                in_reply_to: None,
                references_header: None,
            })
            .unwrap();
        db.send_draft(id).unwrap();
        id
    }

    /// `queue_draft`, but the draft carries its own chosen identity/alias
    /// (S47) — used to prove `apply_send` honours it over the account's own
    /// address passed in via `SendConfig`.
    fn queue_draft_with_identity(
        db: &Database,
        subject: &str,
        from_email: &str,
        from_name: Option<&str>,
    ) -> i64 {
        let id = db
            .save_draft(&NewDraft {
                id: None,
                account_id: "acc".into(),
                to: vec![DraftAddress {
                    name: None,
                    email: "bob@example.com".into(),
                }],
                cc: vec![],
                bcc: vec![],
                subject: subject.into(),
                body_text: "body\n".into(),
                body_html: None,
                from_email: Some(from_email.into()),
                from_name: from_name.map(str::to_owned),
                in_reply_to: None,
                references_header: None,
            })
            .unwrap();
        db.send_draft(id).unwrap();
        id
    }

    /// Like `queue_draft`, but queued with an undo-send window
    /// (`delay_seconds`): the `send` op's `not_before_epoch` is `now +
    /// delay_seconds` instead of immediately due.
    fn queue_draft_delayed(db: &Database, subject: &str, delay_seconds: i64) -> i64 {
        let id = db
            .save_draft(&NewDraft {
                id: None,
                account_id: "acc".into(),
                to: vec![DraftAddress {
                    name: None,
                    email: "bob@example.com".into(),
                }],
                cc: vec![],
                bcc: vec![],
                subject: subject.into(),
                body_text: "body\n".into(),
                body_html: None,
                from_email: None,
                from_name: None,
                in_reply_to: None,
                references_header: None,
            })
            .unwrap();
        db.send_draft_delayed(id, delay_seconds).unwrap();
        id
    }

    /// Records submissions; fails on demand.
    #[derive(Default)]
    struct MockSender {
        sent: Vec<(String, Vec<String>, Vec<u8>)>,
        fail: bool,
    }

    impl SmtpSender for MockSender {
        fn send(&mut self, from: &str, rcpts: &[String], raw: &[u8]) -> Result<(), ProviderError> {
            if self.fail {
                return Err(ProviderError::Network("smtp down".into()));
            }
            self.sent
                .push((from.to_string(), rcpts.to_vec(), raw.to_vec()));
            Ok(())
        }
    }

    /// Always fails with a specific `ProviderError` — for asserting that each
    /// variant lands in `pending_operations.last_error` under the matching
    /// kind prefix (: ComposeView/AccountSyncStatus classify
    /// send failures the same way sync failures are classified).
    struct FixedFailSender(fn(String) -> ProviderError);
    impl SmtpSender for FixedFailSender {
        fn send(
            &mut self,
            _from: &str,
            _rcpts: &[String],
            _raw: &[u8],
        ) -> Result<(), ProviderError> {
            Err((self.0)("simulated failure".into()))
        }
    }

    /// Minimal read-only provider for driving the IMAP replayer in tests.
    /// Note: `append` keeps its default (unsupported error), so a `sent_copy`
    /// op replayed through this provider FAILS — used to prove copy failures
    /// never leak into send state.
    struct NoopProvider;
    impl MailProvider for NoopProvider {
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
    }

    /// Records `append` calls, for asserting the Sent-copy replay.
    #[derive(Default)]
    struct AppendRecorder {
        appends: Vec<(String, String, Vec<u8>)>,
    }
    impl MailProvider for AppendRecorder {
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
            mailbox_path: &str,
            message_id: &str,
            raw: &[u8],
        ) -> Result<(), ProviderError> {
            self.appends.push((
                mailbox_path.to_string(),
                message_id.to_string(),
                raw.to_vec(),
            ));
            Ok(())
        }
    }

    #[test]
    fn send_uses_the_drafts_own_identity_when_it_chose_one() {
        let db = temp_db();
        seed_account(&db);
        queue_draft_with_identity(
            &db,
            "From an alias",
            "alias@example.com",
            Some("Alias Person"),
        );

        let mut sender = MockSender::default();
        // SendConfig's own from_email/from_name (the account's address) must
        // be OVERRIDDEN by the draft's own choice.
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", Some("Alice"), &mut sender)
                .unwrap();
        assert_eq!(report.sent, 1);
        assert_eq!(
            sender.sent[0].0, "alias@example.com",
            "envelope from must be the alias"
        );
        let raw = String::from_utf8_lossy(&sender.sent[0].2);
        assert!(
            raw.contains("From: \"Alias Person\" <alias@example.com>"),
            "raw:\n{raw}"
        );
        assert!(
            !raw.contains("alice@example.com"),
            "the account's own address must not appear"
        );
    }

    #[test]
    fn an_identitys_reply_to_and_auto_bcc_apply_at_delivery_time() {
        let db = temp_db();
        seed_account(&db);
        db.create_identity(&castellum_models::NewIdentity {
            account_id: "acc".into(),
            email: "work@example.com".into(),
            display_name: Some("Alice (work)".into()),
            reply_to: Some("team@example.com".into()),
            bcc: Some("archive@example.com".into()),
            is_default: false,
        })
        .unwrap();
        queue_draft_with_identity(&db, "From work", "WORK@example.com", Some("Alice (work)"));
        let mut sender = MockSender::default();
        send_pending_with_sender(&db, "acc", "alice@example.com", Some("Alice"), &mut sender)
            .unwrap();
        let (from, rcpts, raw) = &sender.sent[0];
        assert_eq!(from, "WORK@example.com");
        let raw = String::from_utf8_lossy(raw);
        assert!(raw.contains("Reply-To: team@example.com"), "raw:\n{raw}");
        // The auto-Bcc rides the ENVELOPE only.
        assert!(
            rcpts.iter().any(|r| r == "archive@example.com"),
            "{rcpts:?}"
        );
        assert!(
            !raw.contains("archive@example.com"),
            "Bcc must never reach the headers:\n{raw}"
        );
    }

    #[test]
    fn auto_bcc_is_not_added_twice_and_the_account_address_carries_neither() {
        let db = temp_db();
        seed_account(&db);
        db.create_identity(&castellum_models::NewIdentity {
            account_id: "acc".into(),
            email: "work@example.com".into(),
            display_name: None,
            reply_to: None,
            bcc: Some("bob@example.com".into()), // already a To recipient
            is_default: false,
        })
        .unwrap();
        queue_draft_with_identity(&db, "dup", "work@example.com", None);
        queue_draft(&db, "plain account address"); // no identity chosen
        let mut sender = MockSender::default();
        send_pending_with_sender(&db, "acc", "alice@example.com", Some("Alice"), &mut sender)
            .unwrap();
        let identity_send = sender
            .sent
            .iter()
            .find(|(from, _, _)| from == "work@example.com")
            .unwrap();
        assert_eq!(
            identity_send
                .1
                .iter()
                .filter(|r| *r == "bob@example.com")
                .count(),
            1
        );
        let account_send = sender
            .sent
            .iter()
            .find(|(from, _, _)| from == "alice@example.com")
            .unwrap();
        assert!(!String::from_utf8_lossy(&account_send.2).contains("Reply-To:"));
    }

    #[test]
    fn send_falls_back_to_the_account_address_when_the_draft_chose_no_identity() {
        let db = temp_db();
        seed_account(&db);
        queue_draft(&db, "No identity chosen"); // pre-S47 shape: from_email = None

        let mut sender = MockSender::default();
        send_pending_with_sender(&db, "acc", "alice@example.com", Some("Alice"), &mut sender)
            .unwrap();
        assert_eq!(
            sender.sent[0].0, "alice@example.com",
            "unchanged pre-S47 behaviour"
        );
    }

    #[test]
    fn send_delivers_marks_sent_and_is_idempotent() {
        let db = temp_db();
        seed_account(&db);
        let id = queue_draft(&db, "Hello");

        let mut sender = MockSender::default();
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", Some("Alice"), &mut sender)
                .unwrap();
        assert_eq!(report.sent, 1);
        assert_eq!(report.failed, 0);

        // Envelope has the Bcc recipient; headers (raw bytes) do not.
        let (from, rcpts, raw) = &sender.sent[0];
        assert_eq!(from, "alice@example.com");
        assert_eq!(
            rcpts,
            &vec![
                "bob@example.com".to_string(),
                "hidden@example.com".to_string()
            ]
        );
        assert!(!String::from_utf8_lossy(raw).contains("hidden@example.com"));

        // Draft delivered; the send op is done. What remains pending is exactly
        // the queued Sent-copy (drained later by the IMAP replayer, not here).
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "sent");
        let ops = db.pending_operations("acc").unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].kind, "sent_copy");

        // Second run: nothing due — a no-op, no re-submission, no second copy.
        let again =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        assert_eq!(again.sent, 0);
        assert_eq!(sender.sent.len(), 1);
        assert_eq!(db.pending_operations("acc").unwrap().len(), 1);
    }

    #[test]
    fn a_successful_send_proposes_one_waiting_for_reply_and_a_failed_one_none() {
        let db = temp_db();
        seed_account(&db);
        let id = queue_draft(&db, "Quote please");
        // A failing submit proposes nothing.
        let mut failing = MockSender {
            fail: true,
            ..Default::default()
        };
        send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut failing).unwrap();
        assert_eq!(db.count_proposed_suggestions().unwrap(), 0);
        // The first retry backoff is zero, so the next drain delivers.
        let mut sender = MockSender::default();
        send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        // Draining again does not duplicate it.
        send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        let all = db.list_suggestions(&Default::default()).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].kind, "waiting_for_reply");
        assert_eq!(all[0].status, "proposed");
        assert_eq!(all[0].payload["title"], "Quote please");
        assert!(all[0].rfc822_message_id.is_some());
        assert_eq!(all[0].dedupe_key, format!("sent:acc:{id}"));
        // Proposal only: no work item exists.
        assert!(db
            .list_work_items_filtered(&Default::default())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn evaluation_mode_blocks_send_and_leaves_the_draft_queued() {
        let db = temp_db();
        seed_account(&db); // enables writes
        let id = queue_draft(&db, "Hello"); // send op queued while writable
        db.set_account_write_enabled("acc", false).unwrap(); // evaluation mode

        let mut sender = MockSender::default();
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        assert_eq!(report.sent, 0, "nothing is delivered in evaluation mode");
        assert_eq!(report.blocked, 1, "the queued send is reported blocked");
        assert!(sender.sent.is_empty(), "no message reaches the SMTP server");
        // Draft is untouched (not sent), op stays queued, no attempt consumed.
        assert_ne!(db.get_draft(id).unwrap().unwrap().status, "sent");
        let ops = db.pending_operations("acc").unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].kind, SEND_KIND);
        assert_eq!(ops[0].attempts, 0, "no attempt consumed");

        // Re-enabling writes lets the draft deliver.
        db.set_account_write_enabled("acc", true).unwrap();
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        assert_eq!(report.sent, 1);
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "sent");
    }

    #[test]
    fn smtp_success_enqueues_sent_copy_and_replay_appends_it() {
        let db = temp_db();
        seed_account(&db);
        queue_draft(&db, "Keep a copy");

        let mut sender = MockSender::default();
        send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();

        // The sent_copy payload is self-contained: Sent path (auto-created —
        // the account had no sent mailbox), the delivered bytes, and the
        // generated Message-ID from the raw headers.
        let ops = db.pending_operations("acc").unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].kind, "sent_copy");
        let payload: serde_json::Value = serde_json::from_str(&ops[0].payload_json).unwrap();
        assert_eq!(payload["dest_path"], "Sent");
        let msgid = payload["message_id"].as_str().unwrap().to_string();
        assert!(msgid.starts_with("<castellum."), "got {msgid}");
        let raw_sent = &sender.sent[0].2;
        assert!(String::from_utf8_lossy(raw_sent).contains(&format!("Message-ID: {msgid}")));

        // The IMAP replayer drains it: one APPEND of exactly the sent bytes.
        let mut provider = AppendRecorder::default();
        let report = replay_with_provider(&db, "acc", &mut provider).unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(report.failed, 0);
        assert_eq!(provider.appends.len(), 1);
        let (path, appended_msgid, appended_raw) = &provider.appends[0];
        assert_eq!(path, "Sent");
        assert_eq!(appended_msgid, &msgid);
        assert_eq!(appended_raw, raw_sent, "byte-identical to the delivery");
        assert!(db.pending_operations("acc").unwrap().is_empty());
    }

    #[test]
    fn sent_copy_failure_is_never_a_send_failure() {
        // The mail is out; a failing Sent-copy must stay on ITS op (retried by
        // the replayer, visible in the outbox) and never resubmit or flip the
        // draft's sent status.
        let db = temp_db();
        seed_account(&db);
        let id = queue_draft(&db, "Copy fails");

        let mut sender = MockSender::default();
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        assert_eq!(report.sent, 1, "send itself succeeded");

        // Replay through a provider without APPEND support: the copy fails.
        let mut provider = NoopProvider;
        let replay = replay_with_provider(&db, "acc", &mut provider).unwrap();
        assert_eq!(replay.failed, 1);
        let ops = db.pending_operations("acc").unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].kind, "sent_copy");
        assert_eq!(ops[0].status, "failed");
        assert!(ops[0]
            .last_error
            .as_deref()
            .unwrap()
            .contains("append not supported"));

        // Send state is untouched: draft stays sent, nothing resubmits.
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "sent");
        let again =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        assert_eq!(again.sent, 0);
        assert_eq!(again.failed, 0, "the failed copy op is not a send op");
        assert_eq!(sender.sent.len(), 1, "no second SMTP submission");
    }

    #[test]
    fn failure_keeps_draft_in_outbox_and_retries() {
        let db = temp_db();
        seed_account(&db);
        let id = queue_draft(&db, "Flaky");

        let mut down = MockSender {
            fail: true,
            ..Default::default()
        };
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut down).unwrap();
        assert_eq!(report.sent, 0);
        assert_eq!(report.failed, 1);

        // Op failed with the error recorded; draft still queued.
        let ops = db.pending_operations("acc").unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].status, "failed");
        assert_eq!(ops[0].attempts, 1);
        let recorded = ops[0].last_error.as_deref().unwrap();
        assert!(recorded.contains("smtp down"));
        // Classified with the stable "<kind>: " prefix the Swift side reads
        // (ComposeView/SendFailurePresentation) — never the raw detail alone.
        assert!(
            recorded.starts_with("network: "),
            "expected a classified prefix, got {recorded:?}"
        );
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "outbox");

        // SMTP recovers: backoff(1) == 0, so the next run delivers (leaving
        // only the queued Sent-copy for the IMAP replayer).
        let mut up = MockSender::default();
        let retry =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut up).unwrap();
        assert_eq!(retry.sent, 1);
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "sent");
        let ops = db.pending_operations("acc").unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].kind, "sent_copy");
    }

    #[test]
    fn send_op_exhausted_at_crash_ceiling_is_failed_permanent() {
        // A `send` op whose attempts reached the cap (e.g. repeated crashes) must
        // be retired to the terminal `failed_permanent` state by the send drain's
        // crash recovery — visible in the outbox, never retried.
        //
        // Since a send found `running` AT LAUNCH is held, not
        // retried, so repeated crashes no longer walk a send up to the cap by
        // themselves; this retirement remains for an exhausted `running` row
        // met after the launch pass (reconciled first, below).
        let db = temp_db();
        seed_account(&db);
        db.reconcile_undo_sends("acc").unwrap();
        queue_draft(&db, "Doomed");
        let id = db.pending_operations("acc").unwrap()[0].id;

        for _ in 0..MAX_ATTEMPTS {
            db.set_operation_status(id, "running", None).unwrap();
        }

        let mut up = MockSender::default();
        send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut up).unwrap();
        assert_eq!(up.sent.len(), 0, "terminal op is not submitted");

        let ops = db.pending_operations("acc").unwrap();
        assert_eq!(ops[0].status, "failed_permanent");
        assert!(!due_for_attempt(&ops[0], now_epoch()));
    }

    #[test]
    fn a_drain_never_retires_a_send_still_running_its_last_attempt() {
        // review: drain A claims a send on its LAST allowed
        // attempt (running, attempts == cap) and is mid-SMTP. Drain B's
        // snapshot sees an exhausted `running` op and used to retire it to
        // failed_permanent — after which Send Now (retry → queued → claim)
        // submitted the same message a second time while A was still sending.
        let db = temp_db();
        seed_account(&db);
        // The once-per-launch hold pass has run: every `running` send below
        // belongs to THIS launch.
        db.reconcile_undo_sends("acc").unwrap();
        let draft_id = queue_draft(&db, "Last try in flight");
        let id = db.pending_operations("acc").unwrap()[0].id;
        for _ in 0..(MAX_ATTEMPTS - 1) {
            db.set_operation_status(id, "running", None).unwrap();
        }
        // Drain A's claim (attempts reaches the cap).
        assert!(db.begin_operation_attempt_from(id, "running").unwrap());
        assert_eq!(
            db.pending_operations("acc").unwrap()[0].attempts,
            MAX_ATTEMPTS
        );

        // Drain B runs while A is still submitting: no retirement, no send.
        let mut b = MockSender::default();
        drain(&db, &mut b);
        assert_eq!(b.sent.len(), 0);
        assert_eq!(
            db.pending_operations("acc").unwrap()[0].status,
            "running",
            "an op this launch is still running must not be retired"
        );

        // So Send Now cannot requeue and re-claim it either.
        let mut now = MockSender::default();
        assert_eq!(send_one(&db, &mut now, draft_id).sent, 0);
        assert_eq!(now.sent.len(), 0, "the message must not be submitted twice");
        assert!(!db.retry_operation(id).unwrap());

        // Once A records its outcome, the exhausted op retires as before.
        db.set_operation_status(id, "failed", Some("smtp: 451"))
            .unwrap();
        drain(&db, &mut b);
        let op = db.pending_operations("acc").unwrap()[0].clone();
        assert_eq!(op.status, "failed_permanent");
        assert_eq!(op.last_error.as_deref(), Some("smtp: 451"));
    }

    #[test]
    fn send_execution_failure_at_cap_becomes_failed_permanent() {
        // Execution arm (not crash recovery): a send op at attempts == cap-1 that
        // fails one more real SMTP attempt lands terminal and is counted.
        let db = temp_db();
        seed_account(&db);
        db.reconcile_undo_sends("acc").unwrap(); // launch pass already ran
        queue_draft(&db, "Last try");
        let id = db.pending_operations("acc").unwrap()[0].id;
        for _ in 0..(MAX_ATTEMPTS - 1) {
            db.set_operation_status(id, "running", None).unwrap();
        }

        let mut down = MockSender {
            fail: true,
            ..Default::default()
        };
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut down).unwrap();
        assert_eq!(report.failed, 1);
        let ops = db.pending_operations("acc").unwrap();
        assert_eq!(ops[0].status, "failed_permanent");
        assert_eq!(ops[0].attempts, MAX_ATTEMPTS);
    }

    /// A draft with one attachment, queued for delivery `delay_seconds` out.
    fn queue_draft_with_attachment(db: &Database, subject: &str, delay_seconds: i64) -> i64 {
        let id = db
            .save_draft(&NewDraft {
                id: None,
                account_id: "acc".into(),
                to: vec![DraftAddress {
                    name: None,
                    email: "bob@example.com".into(),
                }],
                cc: vec![],
                bcc: vec![],
                subject: subject.into(),
                body_text: "body\n".into(),
                body_html: None,
                from_email: None,
                from_name: None,
                in_reply_to: None,
                references_header: None,
            })
            .unwrap();
        db.add_draft_attachment(
            id,
            "report.pdf",
            "application/pdf",
            false,
            None,
            b"pdf bytes",
        )
        .unwrap();
        db.send_draft_delayed(id, delay_seconds).unwrap();
        id
    }

    #[test]
    fn undo_send_preserves_the_drafts_attachments() {
        let db = temp_db();
        seed_account(&db);
        let id = queue_draft_with_attachment(&db, "Undo me with attachment", 3600);

        assert!(db.undo_send(id).unwrap(), "undone in time");
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "draft");
        let attachments = db.list_draft_attachments(id).unwrap();
        assert_eq!(
            attachments.len(),
            1,
            "undo must not touch draft_attachments"
        );
        assert_eq!(attachments[0].filename, "report.pdf");
        assert_eq!(
            db.draft_attachment_bytes(attachments[0].id)
                .unwrap()
                .unwrap(),
            b"pdf bytes"
        );

        // Editable again: the reopened draft accepts further attachments.
        db.add_draft_attachment(id, "second.txt", "text/plain", false, None, b"more")
            .unwrap();
        assert_eq!(db.list_draft_attachments(id).unwrap().len(), 2);

        // And a drain never delivers the undone draft.
        let mut sender = MockSender::default();
        send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        assert!(sender.sent.is_empty());
    }

    #[test]
    fn a_delivered_message_with_an_attachment_carries_it_in_the_raw_bytes_and_the_sent_copy() {
        let db = temp_db();
        seed_account(&db);
        let id = queue_draft_with_attachment(&db, "Send me with attachment", 0);

        let mut sender = MockSender::default();
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        assert_eq!(report.sent, 1);
        let raw = sender.sent[0].2.clone();
        assert!(String::from_utf8_lossy(&raw)
            .to_ascii_lowercase()
            .contains("multipart/mixed"));
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "sent");

        // Crash-safety: the durable Sent-copy op carries these exact
        // multipart bytes, so the IMAP APPEND is byte-identical.
        let mut provider = AppendRecorder::default();
        let report = replay_with_provider(&db, "acc", &mut provider).unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(provider.appends[0].2, raw);
    }

    #[test]
    fn a_failed_send_with_an_attachment_retries_with_the_attachment_intact() {
        let db = temp_db();
        seed_account(&db);
        let id = queue_draft_with_attachment(&db, "Flaky with attachment", 0);

        let mut down = MockSender {
            fail: true,
            ..Default::default()
        };
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut down).unwrap();
        assert_eq!(report.failed, 1);
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "outbox");
        assert_eq!(db.list_draft_attachments(id).unwrap().len(), 1);

        let mut up = MockSender::default();
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut up).unwrap();
        assert_eq!(report.sent, 1);
        let raw = String::from_utf8_lossy(&up.sent[0].2).into_owned();
        assert!(raw.contains("report.pdf"), "attachment survives the retry");
    }

    #[test]
    fn imap_replayer_never_touches_send_ops() {
        // A queued send op must be invisible to the IMAP replayer: not applied,
        // not failed (its unknown-kind arm), not even attempted.
        let db = temp_db();
        seed_account(&db);
        queue_draft(&db, "For SMTP only");

        let mut provider = NoopProvider;
        let report = replay_with_provider(&db, "acc", &mut provider).unwrap();
        assert_eq!(report.applied, 0);
        assert_eq!(
            report.failed, 0,
            "send op must not be failed by IMAP replay"
        );

        let ops = db.pending_operations("acc").unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].status, "queued", "untouched");
        assert_eq!(ops[0].attempts, 0, "no attempt consumed");

        // The send executor then drains it normally.
        let mut sender = MockSender::default();
        let sent =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        assert_eq!(sent.sent, 1);
    }

    #[test]
    fn already_sent_draft_is_not_resubmitted() {
        // Simulates a crash after SMTP submit + mark_draft_sent_with_copy but
        // before the send op's status update: the retry must clear the op
        // WITHOUT sending again — and without enqueueing a SECOND sent_copy
        // (the crashed attempt already queued one, atomically with the mark).
        let db = temp_db();
        seed_account(&db);
        let id = queue_draft(&db, "Once only");
        db.mark_draft_sent_with_copy(id, "<crashed-attempt@x>", b"delivered bytes")
            .unwrap(); // delivery + copy enqueue already happened

        let mut sender = MockSender::default();
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        assert_eq!(report.sent, 1, "op cleared");
        assert!(sender.sent.is_empty(), "no second SMTP submission");
        let ops = db.pending_operations("acc").unwrap();
        assert_eq!(ops.len(), 1, "exactly the one sent_copy from the crash");
        assert_eq!(ops[0].kind, "sent_copy");
    }

    #[test]
    fn unsendable_draft_marks_op_failed() {
        // A draft with no recipients can't be built into a message: the op
        // records the failure and the draft stays in the outbox for the user.
        let db = temp_db();
        seed_account(&db);
        let id = db
            .save_draft(&NewDraft {
                id: None,
                account_id: "acc".into(),
                to: vec![],
                cc: vec![],
                bcc: vec![],
                subject: "No one".into(),
                body_text: "".into(),
                body_html: None,
                from_email: None,
                from_name: None,
                in_reply_to: None,
                references_header: None,
            })
            .unwrap();
        db.send_draft(id).unwrap();

        let mut sender = MockSender::default();
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        assert_eq!(report.failed, 1);
        assert!(sender.sent.is_empty());
        let ops = db.pending_operations("acc").unwrap();
        assert!(ops[0]
            .last_error
            .as_deref()
            .unwrap()
            .contains("no recipients"));
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "outbox");
    }

    #[test]
    fn send_executor_ignores_cancelled_ops() {
        // Defensive: a `cancelled` op (terminal) must never be delivered, even
        // though no purge path currently cancels send ops.
        let db = temp_db();
        seed_account(&db);
        let id = queue_draft(&db, "Never goes out");
        let op_id = db.pending_operations("acc").unwrap()[0].id;
        db.set_operation_status(op_id, "cancelled", Some("test"))
            .unwrap();

        let mut sender = MockSender::default();
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        assert_eq!(report.sent, 0);
        assert_eq!(report.failed, 0);
        assert!(sender.sent.is_empty(), "no SMTP submission");
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "outbox");
        assert!(db.pending_operations("acc").unwrap().is_empty());
    }

    /// Counts submissions and always refuses them as "SMTP AUTH is switched
    /// off" (MS1).
    struct SmtpAuthDisabledSender {
        attempts: usize,
    }
    impl SmtpSender for SmtpAuthDisabledSender {
        fn send(
            &mut self,
            _from: &str,
            _rcpts: &[String],
            _raw: &[u8],
        ) -> Result<(), ProviderError> {
            self.attempts += 1;
            Err(ProviderError::AuthSmtpDisabled(
                "permanent error (535): 5.7.139 Authentication unsuccessful, \
                 SmtpClientAuthentication is disabled for the Mailbox. Visit \
                 https://aka.ms/smtp_auth_disabled"
                    .into(),
            ))
        }
    }

    #[test]
    fn a_permanent_rejection_stops_automatic_retries_after_one_attempt() {
        // G8 (#3632): a 5xx refusal of the message itself (unknown
        // recipient, policy) is terminal at once instead of eight backoff
        // attempts over hours; the manual Retry still requeues it.
        let db = temp_db();
        seed_account(&db);
        let id = queue_draft(&db, "Refused recipient");
        let mut sender = FixedFailSender(ProviderError::Rejected);
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        assert_eq!(report.failed, 1);
        let ops = db.pending_operations("acc").unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].status, "failed_permanent");
        assert_eq!(ops[0].attempts, 1);
        assert_eq!(
            ops[0].last_error.as_deref(),
            Some("rejected: simulated failure")
        );
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "outbox");

        // A transient network failure still backs off and retries.
        let db = temp_db();
        seed_account(&db);
        queue_draft(&db, "Flaky network");
        let mut sender = FixedFailSender(ProviderError::Network);
        send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        assert_eq!(db.pending_operations("acc").unwrap()[0].status, "failed");
    }

    #[test]
    fn a_held_send_is_not_logged_as_a_delivery_failure() {
        // Pin the status the executor actually records for both held paths,
        // then the line that status picks.
        for e in [
            ApplySendError::MaybeSent,
            ApplySendError::Provider(ProviderError::MaybeDelivered("reset".into())),
        ] {
            let status = send_failure_status(&e, 1);
            assert_eq!(status, "held");
            let message = send_failure_log_message(status);
            assert!(!message.contains("delivery failed"), "{message}");
            assert!(message.contains("may have been delivered"), "{message}");
        }
        for status in ["failed", "failed_permanent"] {
            assert_eq!(send_failure_log_message(status), "send: delivery failed");
        }
    }

    #[test]
    fn a_deferred_4xx_is_retried_and_reads_as_deferred_not_network() {
        // A 4xx (greylisting, full spool) comes from a live server: it must
        // keep the backoff retries, and its kind must not be `network`
        // (the app renders that as "Couldn't reach <server>").
        let db = temp_db();
        seed_account(&db);
        let id = queue_draft(&db, "Greylisted");
        let mut sender = FixedFailSender(ProviderError::Deferred);
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        assert_eq!(report.failed, 1);
        let ops = db.pending_operations("acc").unwrap();
        assert_eq!(ops[0].status, "failed");
        assert_eq!(
            ops[0].last_error.as_deref(),
            Some("deferred: simulated failure")
        );
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "outbox");
        assert_eq!(outbox_error_kind("deferred: simulated failure"), "deferred");
    }

    #[test]
    fn smtp_auth_disabled_stops_automatic_retries_after_one_attempt() {
        // The refusal is a mailbox or tenant setting: the remaining seven
        // backoff attempts would each be refused the same way, so the op goes
        // straight to the terminal needs-attention state. The message stays
        // in the Outbox and the manual Retry still works once the setting
        // has been changed.
        let db = temp_db();
        seed_account(&db);
        let id = queue_draft(&db, "Refused by Microsoft");

        let mut sender = SmtpAuthDisabledSender { attempts: 0 };
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        assert_eq!(report.failed, 1);
        assert_eq!(sender.attempts, 1);

        let ops = db.pending_operations("acc").unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].status, "failed_permanent");
        assert_eq!(ops[0].attempts, 1);
        assert!(
            ops[0]
                .last_error
                .as_deref()
                .unwrap()
                .starts_with("auth_smtp_disabled: "),
            "{:?}",
            ops[0].last_error
        );
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "outbox");

        // Later drains (sync nudges, the Send Later sweep) never resubmit it.
        for _ in 0..3 {
            let again =
                send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender)
                    .unwrap();
            assert_eq!(again.sent, 0);
        }
        assert_eq!(sender.attempts, 1, "no automatic retry");

        // The person fixed the setting and pressed Retry in the Outbox.
        db.retry_operation(ops[0].id).unwrap();
        let mut up = MockSender::default();
        let retry =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut up).unwrap();
        assert_eq!(retry.sent, 1);
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "sent");
    }

    #[test]
    fn send_failure_kind_prefix_matches_provider_error_variant() {
        // Every ProviderError variant a send attempt can fail with must record
        // its matching stable kind token — this is the only
        // signal ComposeView's Swift-side presentation is allowed to trust.
        type Ctor = fn(String) -> ProviderError;
        let cases: [(Ctor, &str); 8] = [
            (ProviderError::Auth, "auth"),
            // MS1: must NOT start with "auth:" - that prefix is what makes
            // the app run its forced-OAuth-refresh retry.
            (ProviderError::AuthSmtpDisabled, "auth_smtp_disabled"),
            (ProviderError::Tls, "tls"),
            (ProviderError::Network, "network"),
            (ProviderError::Protocol, "protocol"),
            (ProviderError::Unsupported, "unsupported"),
            (ProviderError::Rejected, "rejected"),
            (ProviderError::Deferred, "deferred"),
        ];
        for (ctor, expected_kind) in cases {
            let db = temp_db();
            seed_account(&db);
            queue_draft(&db, "Kind check");

            let mut sender = FixedFailSender(ctor);
            let report =
                send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender)
                    .unwrap();
            assert_eq!(report.failed, 1, "kind {expected_kind}");

            let ops = db.pending_operations("acc").unwrap();
            let recorded = ops[0].last_error.as_deref().unwrap();
            assert_eq!(
                recorded,
                format!("{expected_kind}: simulated failure"),
                "kind {expected_kind}"
            );
        }
    }

    #[test]
    fn send_config_debug_redacts_smtp_password() {
        let cfg: SendConfig = serde_json::from_str(
            r#"{"account_id":"acc","from_email":"a@x",
                "smtp":{"host":"smtp.x","port":587,
                        "username":"alice","password":"hunter2-super-secret"}}"#,
        )
        .unwrap();
        // SendConfig's derived Debug nests SmtpSettings' redacting Debug.
        let s = format!("{cfg:?}");
        assert!(!s.contains("hunter2-super-secret"), "password leaked: {s}");
        assert!(s.contains("<redacted>"));
        assert!(
            s.contains("smtp.x") && s.contains("alice"),
            "other fields kept: {s}"
        );
    }

    #[test]
    fn send_config_parses_with_defaults() {
        let cfg: SendConfig = serde_json::from_str(
            r#"{
                "account_id": "acc",
                "from_email": "alice@example.com",
                "smtp": {"host": "127.0.0.1", "port": 3025,
                         "username": "u", "password": "p", "use_tls": false}
            }"#,
        )
        .unwrap();
        assert_eq!(cfg.account_id, "acc");
        assert!(cfg.from_name.is_none());
        assert!(!cfg.smtp.use_tls);
        assert!(!cfg.smtp.allow_invalid_certs);

        // use_tls defaults ON (safe default), matching ReplayConfig.
        let cfg2: SendConfig = serde_json::from_str(
            r#"{"account_id":"a","from_email":"e@x.com",
                "smtp":{"host":"h","port":587,"username":"","password":""}}"#,
        )
        .unwrap();
        assert!(cfg2.smtp.use_tls);
    }

    // --- S25: undo-send window ------------------------------------------

    #[test]
    fn skip_before_time_a_delayed_send_is_not_delivered_inside_its_window() {
        let db = temp_db();
        seed_account(&db);
        // Mirrors real usage: `AppModel.bootstrap()` always runs the launch
        // reconcile before anything can be composed/sent (see
        // `reconcile_gate_never_re_holds_a_legitimate_window_...` below for
        // why this makes the delayed send queued next provably fresh, not an
        // ambiguous survivor `send_pending_with_sender`'s own crash-safety
        // pass (S25) would otherwise have to hold).
        db.reconcile_undo_sends("acc").unwrap();
        let id = queue_draft_delayed(&db, "Wait for it", 3600);

        let mut sender = MockSender::default();
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        assert_eq!(report.sent, 0, "still inside the undo window");
        assert_eq!(report.failed, 0);
        assert!(sender.sent.is_empty(), "no SMTP submission");
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "outbox");
        let ops = db.pending_operations("acc").unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].status, "queued");
        assert_eq!(ops[0].attempts, 0, "no attempt consumed while waiting");
        assert!(ops[0].not_before_epoch.unwrap() > now_epoch());
    }

    #[test]
    fn delivers_once_the_undo_window_ends() {
        let db = temp_db();
        seed_account(&db);
        // See the comment in `skip_before_time_...` above: the launch
        // reconcile runs before this window is even opened, so it's provably
        // fresh, not a crash survivor.
        db.reconcile_undo_sends("acc").unwrap();
        let id = queue_draft_delayed(&db, "Deliver after the window", 1);

        let mut sender = MockSender::default();
        // Still inside the 1s window.
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        assert_eq!(report.sent, 0);

        std::thread::sleep(std::time::Duration::from_millis(1100));
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        assert_eq!(report.sent, 1, "window ended: delivered on the next drain");
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "sent");
    }

    #[test]
    fn undo_send_in_time_cancels_the_op_and_returns_the_draft_to_draft() {
        let db = temp_db();
        seed_account(&db);
        let id = queue_draft_delayed(&db, "Undo me", 3600);

        assert!(db.undo_send(id).unwrap(), "undone in time");
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "draft");
        assert!(
            db.pending_operations("acc").unwrap().is_empty(),
            "the send op is cancelled, not just left queued"
        );

        // Proves it, rather than just asserting state: a drain never delivers it.
        let mut sender = MockSender::default();
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        assert_eq!(report.sent, 0);
        assert!(sender.sent.is_empty());

        // A second undo is a benign no-op — nothing left to undo.
        assert!(!db.undo_send(id).unwrap());
    }

    #[test]
    fn undo_send_too_late_once_already_submitted() {
        let db = temp_db();
        seed_account(&db);
        let id = queue_draft(&db, "Already gone"); // delay 0: due immediately

        let mut sender = MockSender::default();
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        assert_eq!(report.sent, 1);
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "sent");

        assert!(!db.undo_send(id).unwrap(), "too late: already delivered");
        assert_eq!(
            db.get_draft(id).unwrap().unwrap().status,
            "sent",
            "undo must never revert a delivered draft"
        );
    }

    #[test]
    fn undo_send_never_races_a_send_the_sender_has_already_claimed() {
        // Same claiming UPDATE the sender uses (begin_operation_attempt): once
        // it has flipped the op to 'running', undo_send must lose cleanly.
        let db = temp_db();
        seed_account(&db);
        let id = queue_draft_delayed(&db, "Race", 3600);
        let op_id = db.pending_operations("acc").unwrap()[0].id;

        assert!(
            db.begin_operation_attempt(op_id).unwrap(),
            "sender claims it"
        );
        assert!(
            !db.undo_send(id).unwrap(),
            "the sender got there first — undo must not win the race"
        );
        assert_eq!(
            db.get_draft(id).unwrap().unwrap().status,
            "outbox",
            "still owned by the in-flight send attempt"
        );
    }

    #[test]
    fn crash_hold_rule_holds_a_delayed_send_with_no_proof_of_continuity() {
        // The backlog's hard rule: never send after an ambiguous crash. A fresh
        // profile has no heartbeat at all (treated as epoch 0), so a queued
        // send's window necessarily postdates it — held, not auto-sent.
        let db = temp_db();
        seed_account(&db);
        let id = queue_draft_delayed(&db, "Interrupted", 3600);

        let held = db.reconcile_undo_sends("acc").unwrap();
        assert_eq!(held, 1);
        let ops = db.pending_operations("acc").unwrap();
        assert_eq!(ops[0].status, "held");
        assert_eq!(
            db.get_draft(id).unwrap().unwrap().status,
            "outbox",
            "not auto-sent"
        );

        // `held` is inert to the normal executor, whatever runs afterwards.
        let mut sender = MockSender::default();
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        assert_eq!(report.sent, 0);
        assert!(
            sender.sent.is_empty(),
            "never auto-sent after an ambiguous crash"
        );

        // The Outbox "Edit" action: undo_send still works on a held op.
        assert!(db.undo_send(id).unwrap());
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "draft");
    }

    #[test]
    fn crash_hold_rule_leaves_a_send_a_clean_heartbeat_already_covers() {
        // A queued send whose window already ended at or before a KNOWN clean
        // drain (e.g. it stayed queued only because writes were disabled) is
        // not ambiguous — must not be held.
        let db = temp_db();
        seed_account(&db);
        let id = queue_draft_delayed(&db, "Blocked, not crashed", 1);
        db.touch_last_clean_drain(now_epoch() + 100).unwrap();

        let held = db.reconcile_undo_sends("acc").unwrap();
        assert_eq!(held, 0);
        assert_eq!(db.pending_operations("acc").unwrap()[0].status, "queued");
        let _ = id;
    }

    #[test]
    fn send_now_releases_a_held_send_for_the_next_drain() {
        let db = temp_db();
        seed_account(&db);
        let id = queue_draft_delayed(&db, "Held then released", 3600);
        db.reconcile_undo_sends("acc").unwrap();
        assert_eq!(db.pending_operations("acc").unwrap()[0].status, "held");

        assert!(db.send_now(id).unwrap());
        let ops = db.pending_operations("acc").unwrap();
        assert_eq!(ops[0].status, "queued");
        assert!(ops[0].not_before_epoch.is_none(), "undo window cleared");

        let mut sender = MockSender::default();
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        assert_eq!(report.sent, 1);
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "sent");

        // A non-held op is a benign no-op for send_now.
        assert!(!db.send_now(id).unwrap());
    }

    #[test]
    fn send_drain_holds_an_interrupted_delayed_send_even_without_an_explicit_reconcile_first() {
        // Regression for the S25 relaunch race: `send_pending_with_sender` used
        // to trust a `queued` op's own `not_before_epoch` gate alone. If a drain
        // ran BEFORE the launch-time reconcile (`AppModel.
        // reconcileUndoSendsThenReplay`, off-actor and asynchronous) had
        // completed — e.g. an immediate Send racing ahead of it right after
        // relaunch — and the interrupted op's window had already elapsed by
        // then, `due_for_attempt` would say it's due and it would be claimed
        // and delivered: exactly the "auto-send after an ambiguous crash" the
        // hard rule forbids. `send_pending_with_sender` now runs the hold pass
        // itself before claiming anything (`Database::reconcile_undo_sends`'s
        // once-per-account gate makes this safe to call unconditionally), so
        // THIS drain alone — with no separate `reconcile_undo_sends` call ever
        // made — must hold it, not send it.
        let db = temp_db();
        seed_account(&db);
        let id = queue_draft_delayed(&db, "Interrupted before relaunch", 1);

        // "The crash": nothing ever touches the heartbeat for this window (a
        // fresh `Database` starts with none at all — epoch 0), and the window
        // elapses with nothing having drained it.
        std::thread::sleep(std::time::Duration::from_millis(1100));

        // "Relaunch, and a send drain runs first" — simulated directly: this is
        // the very first call against this `Database`, with no prior call to
        // `reconcile_undo_sends` of any kind.
        let mut sender = MockSender::default();
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();

        assert_eq!(
            report.sent, 0,
            "must not auto-send an interrupted delayed send"
        );
        assert!(sender.sent.is_empty(), "no SMTP submission");
        let ops = db.pending_operations("acc").unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].status, "held", "the drain itself must hold it");
        assert_eq!(
            db.get_draft(id).unwrap().unwrap().status,
            "outbox",
            "not auto-sent"
        );

        // A later call for the SAME account (e.g. the Swift-side launch
        // reconcile finally landing, or another unrelated Send's own drain)
        // must not re-evaluate it: the gate hands the actual SQL pass to the
        // first caller only.
        let held_again = db.reconcile_undo_sends("acc").unwrap();
        assert_eq!(
            held_again, 0,
            "the hold pass is guaranteed to run at most once per account per launch"
        );
        assert_eq!(db.pending_operations("acc").unwrap()[0].status, "held");
    }

    #[test]
    fn reconcile_gate_never_re_holds_a_legitimate_window_queued_after_the_one_time_reconcile() {
        // The flip side of the race fix: gating `reconcile_undo_sends` to run
        // once per account must not cost correctness the other way — a brand
        // new, legitimate delayed send queued AFTER that one-time reconcile
        // has already run must stay undoable through an unrelated later drain
        // for the same account, never re-caught by the hold pass.
        let db = temp_db();
        seed_account(&db);

        // The launch-time reconcile runs first — nothing queued yet, a no-op —
        // exactly as it does for a normal launch with no crash to recover from.
        assert_eq!(db.reconcile_undo_sends("acc").unwrap(), 0);

        // A brand-new, legitimate delayed send is queued mid-session.
        let id = queue_draft_delayed(&db, "Fresh undo window", 3600);

        // An unrelated drain for the same account (e.g. a different draft's
        // immediate Send) must NOT re-run the hold pass and catch this window.
        let mut sender = MockSender::default();
        let report =
            send_pending_with_sender(&db, "acc", "alice@example.com", None, &mut sender).unwrap();
        assert_eq!(report.sent, 0, "still inside its own window");
        let ops = db.pending_operations("acc").unwrap();
        assert_eq!(ops[0].status, "queued", "must stay undoable, not held");
        assert!(
            db.undo_send(id).unwrap(),
            "the window is genuinely still open"
        );
    }

    // ---- Send Later (HP1): scheduled sends on the durable queue ----

    /// A file-backed db path a test can drop and reopen, to simulate quit +
    /// relaunch (a fresh `Database` also resets the once-per-launch gate).
    fn temp_path() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-send-later-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    const T0: i64 = 2_000_000_000; // an arbitrary injected "now"

    /// A draft with an attachment, a cc and a chosen identity, scheduled for
    /// `send_at`.
    fn schedule_draft(db: &Database, subject: &str, send_at: i64) -> i64 {
        let id = db
            .save_draft(&NewDraft {
                id: None,
                account_id: "acc".into(),
                to: vec![DraftAddress {
                    name: None,
                    email: "bob@example.com".into(),
                }],
                cc: vec![DraftAddress {
                    name: None,
                    email: "carol@example.com".into(),
                }],
                bcc: vec![],
                subject: subject.into(),
                body_text: "body\n".into(),
                body_html: None,
                from_email: Some("alias@example.com".into()),
                from_name: Some("Alias".into()),
                in_reply_to: None,
                references_header: None,
            })
            .unwrap();
        db.add_draft_attachment(id, "a.txt", "text/plain", false, None, b"abc")
            .unwrap();
        db.schedule_send(id, send_at).unwrap();
        id
    }

    fn drain_at(db: &Database, sender: &mut MockSender, now: i64) -> SendReport {
        send_pending_with_sender_at(db, "acc", "alice@example.com", None, sender, now).unwrap()
    }

    #[test]
    fn scheduled_send_waits_for_its_time_then_sends_once() {
        let db = temp_db();
        seed_account(&db);
        db.reconcile_undo_sends_at("acc", T0).unwrap();
        let id = schedule_draft(&db, "Later", T0 + 3600);
        let mut sender = MockSender::default();

        let r = drain_at(&db, &mut sender, T0 + 3599);
        assert_eq!(r.sent, 0, "not attempted before send_at");
        assert!(sender.sent.is_empty());
        let ops = db.pending_operations("acc").unwrap();
        assert!(ops[0].scheduled);
        assert_eq!(ops[0].status, "queued");
        assert_eq!(ops[0].attempts, 0);
        assert_eq!(ops[0].not_before_epoch, Some(T0 + 3600));

        let r = drain_at(&db, &mut sender, T0 + 3600);
        assert_eq!(r.sent, 1, "attempted at send_at");
        let r = drain_at(&db, &mut sender, T0 + 3700);
        assert_eq!(r.sent, 0, "delivered exactly once");
        assert_eq!(sender.sent.len(), 1);
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "sent");
    }

    #[test]
    fn scheduled_send_survives_quit_and_relaunch() {
        let path = temp_path();
        let id;
        {
            let db = Database::open(&path).unwrap();
            seed_account(&db);
            db.reconcile_undo_sends_at("acc", T0).unwrap();
            id = schedule_draft(&db, "Survives", T0 + 1800);
            let mut sender = MockSender::default();
            drain_at(&db, &mut sender, T0 + 10);
        }
        // Relaunch: a fresh Database. The undo-send crash hold must NOT
        // swallow a scheduled send, and it still sends at its time.
        let db = Database::open(&path).unwrap();
        let mut sender = MockSender::default();
        let r = drain_at(&db, &mut sender, T0 + 600);
        assert_eq!(r.sent, 0, "still waiting after relaunch");
        let ops = db.pending_operations("acc").unwrap();
        assert_eq!(ops[0].status, "queued", "not held by the undo-send pass");
        let r = drain_at(&db, &mut sender, T0 + 1800);
        assert_eq!(r.sent, 1);
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "sent");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cancel_returns_an_intact_draft() {
        let db = temp_db();
        seed_account(&db);
        db.reconcile_undo_sends_at("acc", T0).unwrap();
        let id = schedule_draft(&db, "Cancel me", T0 + 3600);
        assert!(db.undo_send(id).unwrap());
        let d = db.get_draft(id).unwrap().unwrap();
        assert_eq!(d.status, "draft");
        assert_eq!(d.to.len(), 1);
        assert_eq!(d.cc.len(), 1);
        assert_eq!(d.from_email.as_deref(), Some("alias@example.com"));
        assert_eq!(d.from_name.as_deref(), Some("Alias"));
        assert_eq!(db.list_draft_attachments(id).unwrap().len(), 1);
        assert!(db.pending_operations("acc").unwrap().is_empty());
        let mut sender = MockSender::default();
        assert_eq!(drain_at(&db, &mut sender, T0 + 7200).sent, 0);
    }

    #[test]
    fn reschedule_moves_the_time_and_only_while_waiting() {
        let db = temp_db();
        seed_account(&db);
        db.reconcile_undo_sends_at("acc", T0).unwrap();
        let id = schedule_draft(&db, "Move me", T0 + 3600);
        assert!(db.reschedule_send(id, T0 + 7200).unwrap());
        let mut sender = MockSender::default();
        assert_eq!(
            drain_at(&db, &mut sender, T0 + 3600).sent,
            0,
            "old time ignored"
        );
        assert_eq!(drain_at(&db, &mut sender, T0 + 7200).sent, 1);
        assert!(
            !db.reschedule_send(id, T0 + 9000).unwrap(),
            "a delivered send cannot be rescheduled"
        );
    }

    ///: rescheduling (or Send Now on) a failed scheduled send
    /// must also reset its attempt count, otherwise it inherits the old
    /// failures and one more transient error makes it permanent.
    #[test]
    fn reschedule_and_send_now_reset_attempts_of_a_failed_scheduled_send() {
        let db = temp_db();
        seed_account(&db);
        db.reconcile_undo_sends_at("acc", T0).unwrap();
        let id = schedule_draft(&db, "Flaky", T0 + 3600);
        let op = db.pending_operations("acc").unwrap()[0].id;
        for _ in 0..(MAX_ATTEMPTS - 1) {
            db.set_operation_status(op, "running", None).unwrap();
        }
        db.set_operation_status(op, "failed", Some("smtp down"))
            .unwrap();
        assert_eq!(
            db.pending_operations("acc").unwrap()[0].attempts,
            MAX_ATTEMPTS - 1
        );
        assert!(db.reschedule_send(id, T0 + 7200).unwrap());
        assert_eq!(db.pending_operations("acc").unwrap()[0].attempts, 0);

        db.set_operation_status(op, "running", None).unwrap();
        db.set_operation_status(op, "failed", Some("smtp down"))
            .unwrap();
        assert!(db.send_now(id).unwrap());
        assert_eq!(db.pending_operations("acc").unwrap()[0].attempts, 0);
    }

    #[test]
    fn scheduled_send_in_flight_at_a_crash_is_never_retried_and_is_surfaced() {
        let path = temp_path();
        let id;
        {
            let db = Database::open(&path).unwrap();
            seed_account(&db);
            db.reconcile_undo_sends_at("acc", T0).unwrap();
            id = schedule_draft(&db, "In flight", T0 + 60);
            // The sender claimed it, then the app died before the status flip.
            let op = db.pending_operations("acc").unwrap().remove(0);
            assert!(db.begin_operation_attempt(op.id).unwrap());
        }
        let db = Database::open(&path).unwrap();
        let mut sender = MockSender::default();
        let r = drain_at(&db, &mut sender, T0 + 61);
        assert_eq!(r.sent, 0);
        assert!(sender.sent.is_empty(), "NEVER retried automatically");
        let ops = db.pending_operations("acc").unwrap();
        assert_eq!(ops[0].status, "held");
        assert!(ops[0]
            .last_error
            .as_deref()
            .unwrap()
            .starts_with("may have been sent"));
        // Still never, however long we wait and however often we drain.
        assert_eq!(drain_at(&db, &mut sender, T0 + 100_000).sent, 0);
        assert!(sender.sent.is_empty());
        // Only the user's explicit choice moves it on.
        assert!(db.send_now(id).unwrap());
        assert_eq!(drain_at(&db, &mut sender, T0 + 100_001).sent, 1);
        let _ = std::fs::remove_file(&path);
    }

    ///: the hold is not a Send Later special case. An ordinary
    /// send (instant, or past its undo window) that was mid-submission when
    /// the app died may already be in the recipient's inbox.
    #[test]
    fn instant_send_in_flight_at_a_crash_is_never_resubmitted_automatically() {
        let path = temp_path();
        let id;
        {
            let db = Database::open(&path).unwrap();
            seed_account(&db);
            db.reconcile_undo_sends("acc").unwrap();
            id = queue_draft(&db, "Big attachment");
            // The drain claimed it and began SMTP; then the Mac lost power.
            let op = db.pending_operations("acc").unwrap().remove(0);
            assert!(db.begin_operation_attempt(op.id).unwrap());
        }
        let db = Database::open(&path).unwrap();
        let mut sender = MockSender::default();
        for _ in 0..3 {
            assert_eq!(drain(&db, &mut sender).sent, 0);
        }
        assert!(
            sender.sent.is_empty(),
            "a send whose SMTP outcome is unknown must never be resubmitted"
        );
        let op = db.pending_operations("acc").unwrap().remove(0);
        assert_eq!(op.status, "held");
        let error = op.last_error.unwrap();
        assert!(error.starts_with("may have been sent"), "{error}");
        assert!(
            !error.contains("scheduled"),
            "this was not a scheduled message: {error}"
        );
        // It stays listed for the user, as a message that needs a decision.
        let listed = outbox_sends(&db, "acc", now_epoch()).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, "held");

        // Only the user's explicit Send Now submits it, and exactly once.
        assert!(db.send_now(id).unwrap());
        assert_eq!(drain(&db, &mut sender).sent, 1);
        assert_eq!(drain(&db, &mut sender).sent, 0);
        assert_eq!(sender.sent.len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn held_instant_send_goes_back_to_drafts_without_being_sent() {
        let path = temp_path();
        let id;
        {
            let db = Database::open(&path).unwrap();
            seed_account(&db);
            db.reconcile_undo_sends("acc").unwrap();
            id = queue_draft(&db, "Was it sent?");
            let op = db.pending_operations("acc").unwrap().remove(0);
            assert!(db.begin_operation_attempt(op.id).unwrap());
        }
        let db = Database::open(&path).unwrap();
        assert_eq!(db.reconcile_undo_sends("acc").unwrap(), 1);
        // The user checked Sent, found it there, and takes the draft back.
        assert!(db.undo_send(id).unwrap());
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "draft");
        let mut sender = MockSender::default();
        assert_eq!(drain(&db, &mut sender).sent, 0);
        assert!(sender.sent.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_send_this_launch_is_running_is_not_held_by_a_later_drain() {
        // The hold pass runs once per launch, before any claim. A send
        // claimed afterwards is in flight NOW; holding it would let Send Now
        // requeue a message that is still being submitted.
        let db = temp_db();
        seed_account(&db);
        let mut sender = MockSender::default();
        drain(&db, &mut sender); // first drain: runs the launch pass
        queue_draft(&db, "In flight right now");
        let op = db.pending_operations("acc").unwrap().remove(0);
        assert!(db.begin_operation_attempt(op.id).unwrap());

        assert_eq!(db.reconcile_undo_sends("acc").unwrap(), 0);
        assert_eq!(drain(&db, &mut sender).sent, 0);
        assert_eq!(
            db.pending_operations("acc").unwrap()[0].status,
            "running",
            "still owned by the drain that claimed it"
        );
    }

    #[test]
    fn scheduled_send_missed_by_more_than_ten_minutes_needs_confirmation() {
        let path = temp_path();
        let id;
        {
            let db = Database::open(&path).unwrap();
            seed_account(&db);
            db.reconcile_undo_sends_at("acc", T0).unwrap();
            id = schedule_draft(&db, "Missed", T0 + 3600);
        }
        let db = Database::open(&path).unwrap();
        let mut sender = MockSender::default();
        let late = T0 + 3600 + castellum_db::LATE_SEND_GRACE_SECONDS + 1;
        let r = drain_at(&db, &mut sender, late);
        assert_eq!(r.sent, 0);
        assert!(sender.sent.is_empty(), "no silent late send");
        let ops = db.pending_operations("acc").unwrap();
        assert_eq!(ops[0].status, "needs_confirmation");
        assert_eq!(drain_at(&db, &mut sender, late + 5000).sent, 0);
        // Reschedule is allowed from here; so is Back to Drafts.
        assert!(db.reschedule_send(id, late + 60).unwrap());
        assert_eq!(db.pending_operations("acc").unwrap()[0].status, "queued");
        assert_eq!(drain_at(&db, &mut sender, late + 60).sent, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn scheduled_send_that_failed_then_relaunched_late_needs_confirmation() {
        let path = temp_path();
        let id;
        {
            let db = Database::open(&path).unwrap();
            seed_account(&db);
            db.reconcile_undo_sends_at("acc", T0).unwrap();
            id = schedule_draft(&db, "Failed once", T0 + 3600);
            // SMTP is down at its time: the first attempt fails.
            let mut down = MockSender {
                fail: true,
                ..Default::default()
            };
            let r = drain_at(&db, &mut down, T0 + 3600);
            assert_eq!(r.failed, 1);
            assert_eq!(db.pending_operations("acc").unwrap()[0].status, "failed");
        }
        // Quit; relaunch six hours late with SMTP back up.
        let db = Database::open(&path).unwrap();
        let mut sender = MockSender::default();
        let late = T0 + 3600 + 6 * 3600;
        let r = drain_at(&db, &mut sender, late);
        assert_eq!(r.sent, 0);
        assert!(
            sender.sent.is_empty(),
            "no silent late send after a failure"
        );
        assert_eq!(
            db.pending_operations("acc").unwrap()[0].status,
            "needs_confirmation"
        );
        // The failed scheduled op is also reschedulable / cancellable while
        // it is still within the grace (a fresh failure).
        let db2 = temp_db();
        seed_account(&db2);
        db2.reconcile_undo_sends_at("acc", T0).unwrap();
        let f = schedule_draft(&db2, "Fresh failure", T0 + 60);
        let mut down = MockSender {
            fail: true,
            ..Default::default()
        };
        drain_at(&db2, &mut down, T0 + 60);
        assert!(db2.reschedule_send(f, T0 + 120).unwrap());
        drain_at(&db2, &mut down, T0 + 120);
        assert!(db2.undo_send(f).unwrap());
        assert_eq!(db2.get_draft(f).unwrap().unwrap().status, "draft");
        let _ = id;
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn needs_confirmation_offers_send_now_and_back_to_drafts() {
        let db = temp_db();
        seed_account(&db);
        db.reconcile_undo_sends_at("acc", T0).unwrap();
        let a = schedule_draft(&db, "A", T0 + 10);
        let b = schedule_draft(&db, "B", T0 + 10);
        let mut sender = MockSender::default();
        let late = T0 + 10 + castellum_db::LATE_SEND_GRACE_SECONDS + 1;
        drain_at(&db, &mut sender, late);
        assert!(db.send_now(a).unwrap());
        assert!(db.undo_send(b).unwrap());
        assert_eq!(db.get_draft(b).unwrap().unwrap().status, "draft");
        assert_eq!(drain_at(&db, &mut sender, late + 1).sent, 1);
        assert_eq!(db.get_draft(a).unwrap().unwrap().status, "sent");
    }

    #[test]
    fn scheduled_send_missed_by_up_to_ten_minutes_still_sends() {
        let path = temp_path();
        {
            let db = Database::open(&path).unwrap();
            seed_account(&db);
            db.reconcile_undo_sends_at("acc", T0).unwrap();
            schedule_draft(&db, "Just late", T0 + 3600);
        }
        let db = Database::open(&path).unwrap();
        let mut sender = MockSender::default();
        let edge = T0 + 3600 + castellum_db::LATE_SEND_GRACE_SECONDS;
        let r = drain_at(&db, &mut sender, edge);
        assert_eq!(r.sent, 1, "exactly ten minutes late is within the grace");
        assert_eq!(sender.sent.len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn send_now_releases_a_waiting_scheduled_send() {
        let db = temp_db();
        seed_account(&db);
        db.reconcile_undo_sends_at("acc", T0).unwrap();
        let id = schedule_draft(&db, "Now please", T0 + 86_400);
        assert!(db.send_now(id).unwrap());
        let mut sender = MockSender::default();
        assert_eq!(drain_at(&db, &mut sender, T0 + 1).sent, 1);
    }

    // --- OB1: the Outbox query, Send Now and permanent failures -------------

    fn send_one(db: &Database, sender: &mut dyn SmtpSender, draft_id: i64) -> SendReport {
        send_draft_now_with_sender_at(
            db,
            "acc",
            draft_id,
            "alice@example.com",
            None,
            sender,
            now_epoch(),
        )
        .unwrap()
    }

    fn drain(db: &Database, sender: &mut dyn SmtpSender) -> SendReport {
        send_pending_with_sender(db, "acc", "alice@example.com", None, sender).unwrap()
    }

    #[test]
    fn a_queued_send_appears_in_the_outbox_query() {
        let db = temp_db();
        seed_account(&db);
        let id = queue_draft(&db, "Waiting to go");

        let rows = outbox_sends(&db, "acc", now_epoch()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].draft_id, id);
        assert_eq!(rows[0].subject, "Waiting to go");
        assert_eq!(rows[0].recipients, vec!["bob@example.com".to_string()]);
        assert_eq!(rows[0].status, "queued");
        assert_eq!(rows[0].attempts, 0);
        assert_eq!(rows[0].error_kind, None);
        assert!(rows[0].automatic_retry);
        assert!(rows[0].operation_id.is_some());
    }

    #[test]
    fn a_failed_send_keeps_its_error_and_stays_listed() {
        let db = temp_db();
        seed_account(&db);
        let id = queue_draft(&db, "Flaky");
        let mut down = MockSender {
            fail: true,
            ..Default::default()
        };
        drain(&db, &mut down);

        let rows = outbox_sends(&db, "acc", now_epoch()).unwrap();
        assert_eq!(rows.len(), 1, "a failed send stays in the Outbox");
        assert_eq!(rows[0].draft_id, id);
        assert_eq!(rows[0].status, "failed");
        assert_eq!(rows[0].attempts, 1);
        // Only the kind crosses the boundary: never the server's own text.
        assert_eq!(rows[0].error_kind.as_deref(), Some("network"));
        assert!(
            rows[0].automatic_retry,
            "a network failure retries by itself"
        );
        let json = serde_json::to_string(&rows).unwrap();
        assert!(!json.contains("smtp down"), "raw detail leaked: {json}");
        // The reason is durable: still there on a second look.
        let again = outbox_sends(&db, "acc", now_epoch()).unwrap();
        assert_eq!(again[0].error_kind.as_deref(), Some("network"));
    }

    #[test]
    fn send_now_attempts_exactly_that_message() {
        let db = temp_db();
        seed_account(&db);
        let first = queue_draft(&db, "First");
        let second = queue_draft(&db, "Second");

        let mut up = MockSender::default();
        let report = send_one(&db, &mut up, second);
        assert_eq!(report.sent, 1);
        assert_eq!(up.sent.len(), 1, "exactly one message was submitted");
        assert!(String::from_utf8_lossy(&up.sent[0].2).contains("Subject: Second"));
        assert_eq!(db.get_draft(second).unwrap().unwrap().status, "sent");
        // The other message was not touched: still queued, no attempt spent.
        assert_eq!(db.get_draft(first).unwrap().unwrap().status, "outbox");
        let rows = outbox_sends(&db, "acc", now_epoch()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].draft_id, first);
        assert_eq!(rows[0].attempts, 0);
    }

    #[test]
    fn a_permanent_failure_is_not_retried_automatically_but_send_now_still_works() {
        let db = temp_db();
        seed_account(&db);
        let id = queue_draft(&db, "Refused");

        // An authentication refusal is permanent on the FIRST attempt.
        let mut refused = FixedFailSender(ProviderError::Auth);
        assert_eq!(drain(&db, &mut refused).failed, 1);
        let rows = outbox_sends(&db, "acc", now_epoch()).unwrap();
        assert_eq!(rows[0].status, "failed_permanent");
        assert_eq!(rows[0].error_kind.as_deref(), Some("auth"));
        assert!(!rows[0].automatic_retry);
        assert_eq!(rows[0].attempts, 1);

        // Automatic drains (launch, network back, timers) leave it alone, even
        // with a server that would now accept it and long after any backoff.
        let mut up = MockSender::default();
        let far_future = now_epoch() + 7 * 86_400;
        let auto =
            send_pending_with_sender_at(&db, "acc", "alice@example.com", None, &mut up, far_future)
                .unwrap();
        assert_eq!((auto.sent, auto.failed), (0, 0));
        assert!(
            up.sent.is_empty(),
            "a permanent failure must not be retried"
        );
        assert_eq!(outbox_sends(&db, "acc", far_future).unwrap()[0].attempts, 1);

        // Send Now is the user's explicit choice, so it still tries.
        assert_eq!(send_one(&db, &mut up, id).sent, 1);
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "sent");
        assert!(outbox_sends(&db, "acc", now_epoch()).unwrap().is_empty());
    }

    #[test]
    fn a_stale_snapshot_cannot_claim_a_send_someone_else_already_took() {
        // A drain's snapshot saw the op `queued`; meanwhile Send Now claimed
        // it. The drain must skip it, not submit the message a second time.
        let db = temp_db();
        seed_account(&db);
        queue_draft(&db, "Once only");
        let stale = db.pending_operations("acc").unwrap().remove(0);
        assert!(db.begin_operation_attempt(stale.id).unwrap());

        let mut up = MockSender::default();
        let claimed = attempt_send_op(
            &db,
            "acc",
            "alice@example.com",
            None,
            false,
            &mut up,
            &stale,
        )
        .unwrap();
        assert_eq!(claimed, None);
        assert!(up.sent.is_empty());
    }

    #[test]
    fn a_launch_drain_attempts_each_queued_send_once() {
        let db = temp_db();
        seed_account(&db);
        queue_draft(&db, "One");
        queue_draft(&db, "Two");

        let mut up = MockSender::default();
        assert_eq!(drain(&db, &mut up).sent, 2);
        // A second pass (for example the network-return trigger firing right
        // after launch) finds nothing left to attempt.
        let again = drain(&db, &mut up);
        assert_eq!((again.sent, again.failed), (0, 0));
        assert_eq!(
            up.sent.len(),
            2,
            "each queued send was submitted exactly once"
        );
    }

    #[test]
    fn send_now_is_refused_on_a_read_only_account() {
        let db = temp_db();
        seed_account(&db);
        let id = queue_draft(&db, "Blocked");
        db.set_account_write_enabled("acc", false).unwrap();

        let mut up = MockSender::default();
        let report = send_one(&db, &mut up, id);
        assert_eq!((report.sent, report.failed, report.blocked), (0, 0, 1));
        assert!(up.sent.is_empty());
        let rows = outbox_sends(&db, "acc", now_epoch()).unwrap();
        assert_eq!(rows[0].attempts, 0, "a refusal spends no attempt");
    }

    #[test]
    fn requeue_failed_sends_gives_send_all_a_fresh_try() {
        let db = temp_db();
        seed_account(&db);
        queue_draft(&db, "Refused");
        let mut refused = FixedFailSender(ProviderError::Auth);
        drain(&db, &mut refused);

        assert_eq!(db.requeue_failed_sends("acc").unwrap(), 1);
        let rows = outbox_sends(&db, "acc", now_epoch()).unwrap();
        assert_eq!(rows[0].status, "queued");
        assert_eq!(rows[0].attempts, 0);
        let mut up = MockSender::default();
        assert_eq!(drain(&db, &mut up).sent, 1);
    }

    #[test]
    fn a_stranded_outbox_draft_is_listed_and_send_now_requeues_it() {
        // A draft left in `outbox` with no live operation (a crash between
        // the two writes; cancelling now returns the draft to Drafts, FP06)
        // must not vanish from the Outbox.
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-send-test-stranded-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        let db = Database::open(&path).expect("open db");
        seed_account(&db);
        let id = queue_draft(&db, "Stranded");
        let op = db.pending_operations("acc").unwrap()[0].id;
        // Simulate the crash: the operation row is gone, the draft stays queued.
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute("DELETE FROM operations WHERE id = ?1", [op])
            .unwrap();

        let rows = outbox_sends(&db, "acc", now_epoch()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "stranded");
        assert_eq!(rows[0].operation_id, None);
        assert!(!rows[0].automatic_retry);

        let mut up = MockSender::default();
        assert_eq!(send_one(&db, &mut up, id).sent, 1);
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "sent");
    }
}
