// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//! Redacted support-bundle generation, the local diagnostics view, and the
//! read-only integrity check.
//!
//! Allowlist-first: every field in the exported bundle is either a compile-time
//! constant, a count, a timestamp written by SQLite's own clock, or a value
//! re-validated against the fixed sets in [`crate::redact`] at read time.
//! Nothing user-generated (subjects, snippets, bodies, drafts, addresses,
//! folder names, server error text, credentials, prompts, AI output, blob
//! bytes) can flow into it: identifiers are replaced with per-bundle ordinal
//! labels, and any persisted diagnostic value that is not in a known set is
//! replaced with `"redacted"` — so even a hostile or legacy database row
//! cannot leak through. Generation is strictly read-only.
//!
//! The LOCAL diagnostics (`local_diagnostics_json`) is a separate, never-exported
//! view that joins the real `accounts`/`mailboxes` tables to reveal identity
//! (email, path) to the user on-device only. Error text is still the fixed
//! redacted phrase — only account/mailbox IDENTITY is real.

use serde::Serialize;

use crate::redact::{sanitize_capability_tokens, KNOWN_ERROR_KINDS, KNOWN_PHASES, KNOWN_SUMMARIES};
use crate::CoreError;
use castellum_db::Database;

/// Bump when the bundle shape changes incompatibly.
const BUNDLE_SCHEMA: u8 = 1;

/// Well-known mailbox roles: protocol-level constants safe to name verbatim.
const KNOWN_ROLES: &[&str] = &["inbox", "drafts", "sent", "archive", "junk", "trash"];

/// Operation statuses the replayer can produce; anything else folds into
/// `other` rather than echoing an unknown string.
const KNOWN_OP_STATUSES: &[&str] = &[
    "queued",
    "running",
    "succeeded",
    "failed",
    "failed_permanent",
    "cancelled",
];

const REDACTED: &str = "redacted";

#[derive(Debug, Serialize)]
pub struct SupportBundle {
    pub bundle_schema: u8,
    pub generated_at: String,
    pub schema_version: i64,
    pub core_version: &'static str,
    pub integrity: IntegrityReport,
    pub accounts: Vec<AccountDiagnostics>,
}

#[derive(Debug, Serialize)]
pub struct IntegrityReport {
    pub ok: bool,
    pub issue_count: usize,
    /// First `PRAGMA quick_check` message when not ok. These reference only
    /// schema object names and page numbers; truncated defensively.
    pub first_issue: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AccountDiagnostics {
    /// Per-bundle ordinal label (`account-1`, ...). Never the id or email.
    pub label: String,
    pub provider: String,
    /// Sanitised IMAP CAPABILITY tokens captured at the last connect.
    pub capabilities: Vec<String>,
    pub last_attempt_at: Option<String>,
    pub last_success_at: Option<String>,
    pub last_error_kind: Option<String>,
    pub last_error: Option<String>,
    pub last_error_phase: Option<String>,
    pub consecutive_failures: i64,
    pub operations: OperationSummary,
    pub mailboxes: Vec<MailboxDiagnostics>,
}

#[derive(Debug, Default, Serialize)]
pub struct OperationSummary {
    pub queued: i64,
    pub running: i64,
    pub succeeded: i64,
    pub failed: i64,
    pub failed_permanent: i64,
    pub cancelled: i64,
    pub other: i64,
}

#[derive(Debug, Serialize)]
pub struct MailboxDiagnostics {
    /// The role name for well-known roles, else a per-bundle ordinal
    /// (`folder-3`). Never the provider path or display name.
    pub label: String,
    pub role: Option<String>,
    pub last_attempt_at: Option<String>,
    pub last_success_at: Option<String>,
    pub last_error_kind: Option<String>,
    pub last_error: Option<String>,
    pub last_error_phase: Option<String>,
    pub consecutive_failures: i64,
    pub total_messages: i64,
    pub messages_with_body: i64,
    pub messages_without_body: i64,
    pub has_sync_cursor: bool,
    /// Messages the sync could not decode and will retry on a backoff (the
    /// decode-skip retry set). A count only, never a UID.
    pub decode_skips_pending: i64,
}

/// Keep a persisted value only if it is one of the known constants; anything
/// else (legacy rows, hand-edited databases) is replaced, never echoed.
fn allow_known(value: Option<String>, known: &[&str]) -> Option<String> {
    value.map(|v| {
        if known.contains(&v.as_str()) {
            v
        } else {
            REDACTED.to_string()
        }
    })
}

/// Timestamps are written exclusively by SQLite's `datetime('now')`
/// (`YYYY-MM-DD HH:MM:SS`). Anything shaped differently is dropped.
fn allow_timestamp(value: Option<String>) -> Option<String> {
    value.filter(|v| {
        v.len() == 19
            && v.bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b':' | b' '))
    })
}

fn operation_summary(counts: &[(String, i64)]) -> OperationSummary {
    let mut s = OperationSummary::default();
    for (status, n) in counts {
        match status.as_str() {
            "queued" => s.queued += n,
            "running" => s.running += n,
            "succeeded" => s.succeeded += n,
            "failed" => s.failed += n,
            "failed_permanent" => s.failed_permanent += n,
            "cancelled" => s.cancelled += n,
            _ => s.other += n,
        }
    }
    debug_assert!(KNOWN_OP_STATUSES.len() == 6);
    s
}

fn db_err<E: std::fmt::Display>(e: E) -> CoreError {
    CoreError::Db(format!("diagnostics: {e}"))
}

/// Assemble the redacted support bundle. Strictly read-only.
pub fn generate_support_bundle(db: &Database) -> Result<SupportBundle, CoreError> {
    let integrity = db.quick_check().map_err(db_err)?;
    let mut accounts = Vec::new();

    for (account_index, account) in db.list_accounts().map_err(db_err)?.iter().enumerate() {
        let state = db.get_account_sync_state(&account.id).map_err(db_err)?;
        let op_counts = db.operation_status_counts(&account.id).map_err(db_err)?;

        let capabilities = db
            .get_imap_capabilities(&account.id)
            .map_err(db_err)?
            .and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok())
            .map(|tokens| sanitize_capability_tokens(&tokens))
            .unwrap_or_default();

        let mailbox_states = db.list_mailbox_sync_states(&account.id).map_err(db_err)?;
        let mut mailboxes = Vec::new();
        let mut folder_ordinal = 0usize;
        for counts in db.list_mailbox_body_counts(&account.id).map_err(db_err)? {
            let role = counts
                .role
                .as_deref()
                .filter(|r| KNOWN_ROLES.contains(r))
                .map(str::to_string);
            let label = match &role {
                Some(role) => role.clone(),
                None => {
                    folder_ordinal += 1;
                    format!("folder-{folder_ordinal}")
                }
            };
            let st = mailbox_states
                .iter()
                .find(|s| s.mailbox_id == counts.mailbox_id);
            mailboxes.push(MailboxDiagnostics {
                label,
                role,
                last_attempt_at: allow_timestamp(st.and_then(|s| s.last_attempt_at.clone())),
                last_success_at: allow_timestamp(st.and_then(|s| s.last_success_at.clone())),
                last_error_kind: allow_known(
                    st.and_then(|s| s.last_error_kind.clone()),
                    KNOWN_ERROR_KINDS,
                ),
                last_error: allow_known(
                    st.and_then(|s| s.last_error_redacted.clone()),
                    KNOWN_SUMMARIES,
                ),
                last_error_phase: allow_known(
                    st.and_then(|s| s.last_error_phase.clone()),
                    KNOWN_PHASES,
                ),
                consecutive_failures: st.map_or(0, |s| s.consecutive_failures),
                total_messages: counts.total_messages,
                messages_with_body: counts.messages_with_body,
                messages_without_body: (counts.total_messages - counts.messages_with_body).max(0),
                has_sync_cursor: counts.has_sync_cursor,
                decode_skips_pending: counts.decode_skips_pending,
            });
        }

        accounts.push(AccountDiagnostics {
            label: format!("account-{}", account_index + 1),
            provider: if account.provider == "imap" {
                "imap".to_string()
            } else {
                "other".to_string()
            },
            capabilities,
            last_attempt_at: allow_timestamp(
                state.as_ref().and_then(|s| s.last_attempt_at.clone()),
            ),
            last_success_at: allow_timestamp(
                state.as_ref().and_then(|s| s.last_success_at.clone()),
            ),
            last_error_kind: allow_known(
                state.as_ref().and_then(|s| s.last_error_kind.clone()),
                KNOWN_ERROR_KINDS,
            ),
            last_error: allow_known(
                state.as_ref().and_then(|s| s.last_error_redacted.clone()),
                KNOWN_SUMMARIES,
            ),
            last_error_phase: allow_known(
                state.as_ref().and_then(|s| s.last_error_phase.clone()),
                KNOWN_PHASES,
            ),
            consecutive_failures: state.as_ref().map_or(0, |s| s.consecutive_failures),
            operations: operation_summary(&op_counts),
            mailboxes,
        });
    }

    Ok(SupportBundle {
        bundle_schema: BUNDLE_SCHEMA,
        generated_at: db.now_utc().map_err(db_err)?,
        schema_version: db.schema_version().map_err(db_err)?,
        core_version: env!("CARGO_PKG_VERSION"),
        integrity: IntegrityReport {
            ok: integrity.ok,
            issue_count: integrity.messages.len(),
            // quick_check messages reference only schema names/page numbers,
            // but they are the one string the allowlist cannot re-validate —
            // so the EXPORTED bundle carries counts only. The full first
            // message stays available via the local-only integrity check.
            first_issue: None,
        },
        accounts,
    })
}

/// The bundle as JSON (the FFI/Swift-facing entry point).
pub fn generate_support_bundle_json(db: &Database) -> Result<String, CoreError> {
    let bundle = generate_support_bundle(db)?;
    serde_json::to_string(&bundle).map_err(|e| CoreError::Db(format!("serialize bundle: {e}")))
}

// ---------------------------------------------------------------------------
// Local-only diagnostics (never exported; identity is real here)
// ---------------------------------------------------------------------------

/// Per-mailbox local diagnostic row: real path + redacted error text.
#[derive(Debug, Serialize)]
pub struct LocalMailboxDiagnostics {
    pub mailbox_id: String,
    /// The real mailbox path — never written to the exported bundle.
    pub path: String,
    pub role: Option<String>,
    pub last_attempt_at: Option<String>,
    pub last_success_at: Option<String>,
    /// Re-validated against [`KNOWN_ERROR_KINDS`]; hostile rows → `"redacted"`.
    pub last_error_kind: Option<String>,
    /// Re-validated against [`KNOWN_SUMMARIES`]; hostile rows → `"redacted"`.
    pub last_error: Option<String>,
    /// Re-validated against [`KNOWN_PHASES`]; hostile rows → `"redacted"`.
    pub last_error_phase: Option<String>,
    pub consecutive_failures: i64,
    pub total_messages: i64,
    pub messages_with_body: i64,
    pub messages_without_body: i64,
    pub has_sync_cursor: bool,
    /// Messages the sync could not decode and will retry on a backoff.
    pub decode_skips_pending: i64,
}

/// Per-account local diagnostic row: real email + redacted error text.
#[derive(Debug, Serialize)]
pub struct LocalAccountDiagnostics {
    pub account_id: String,
    /// The real account email — never written to the exported bundle.
    pub email: String,
    pub provider: String,
    pub last_attempt_at: Option<String>,
    pub last_success_at: Option<String>,
    /// Re-validated against [`KNOWN_ERROR_KINDS`]; hostile rows → `"redacted"`.
    pub last_error_kind: Option<String>,
    /// Re-validated against [`KNOWN_SUMMARIES`]; hostile rows → `"redacted"`.
    pub last_error: Option<String>,
    /// Re-validated against [`KNOWN_PHASES`]; hostile rows → `"redacted"`.
    pub last_error_phase: Option<String>,
    pub consecutive_failures: i64,
    pub mailboxes: Vec<LocalMailboxDiagnostics>,
}

/// The full local-only diagnostics payload.
#[derive(Debug, Serialize)]
pub struct LocalDiagnostics {
    pub accounts: Vec<LocalAccountDiagnostics>,
}

/// Assemble the LOCAL diagnostics view. Joins real identity (email, path)
/// with the durable sync-state rows. Error values are still re-validated
/// against the known sets — hostile persisted rows show `"redacted"`.
/// Strictly read-only. MUST NEVER be written to the exported support bundle.
pub fn local_diagnostics(db: &Database) -> Result<LocalDiagnostics, CoreError> {
    let mut accounts = Vec::new();

    for account in db.list_accounts().map_err(db_err)? {
        let state = db.get_account_sync_state(&account.id).map_err(db_err)?;
        let mailbox_states = db.list_mailbox_sync_states(&account.id).map_err(db_err)?;
        let mut mailboxes = Vec::new();

        for counts in db.list_mailbox_body_counts(&account.id).map_err(db_err)? {
            let st = mailbox_states
                .iter()
                .find(|s| s.mailbox_id == counts.mailbox_id);
            mailboxes.push(LocalMailboxDiagnostics {
                mailbox_id: counts.mailbox_id,
                path: counts.path,
                role: counts.role,
                last_attempt_at: allow_timestamp(st.and_then(|s| s.last_attempt_at.clone())),
                last_success_at: allow_timestamp(st.and_then(|s| s.last_success_at.clone())),
                last_error_kind: allow_known(
                    st.and_then(|s| s.last_error_kind.clone()),
                    KNOWN_ERROR_KINDS,
                ),
                last_error: allow_known(
                    st.and_then(|s| s.last_error_redacted.clone()),
                    KNOWN_SUMMARIES,
                ),
                last_error_phase: allow_known(
                    st.and_then(|s| s.last_error_phase.clone()),
                    KNOWN_PHASES,
                ),
                consecutive_failures: st.map_or(0, |s| s.consecutive_failures),
                total_messages: counts.total_messages,
                messages_with_body: counts.messages_with_body,
                messages_without_body: (counts.total_messages - counts.messages_with_body).max(0),
                has_sync_cursor: counts.has_sync_cursor,
                decode_skips_pending: counts.decode_skips_pending,
            });
        }

        accounts.push(LocalAccountDiagnostics {
            account_id: account.id.clone(),
            email: account.email.clone(),
            provider: account.provider.clone(),
            last_attempt_at: allow_timestamp(
                state.as_ref().and_then(|s| s.last_attempt_at.clone()),
            ),
            last_success_at: allow_timestamp(
                state.as_ref().and_then(|s| s.last_success_at.clone()),
            ),
            last_error_kind: allow_known(
                state.as_ref().and_then(|s| s.last_error_kind.clone()),
                KNOWN_ERROR_KINDS,
            ),
            last_error: allow_known(
                state.as_ref().and_then(|s| s.last_error_redacted.clone()),
                KNOWN_SUMMARIES,
            ),
            last_error_phase: allow_known(
                state.as_ref().and_then(|s| s.last_error_phase.clone()),
                KNOWN_PHASES,
            ),
            consecutive_failures: state.as_ref().map_or(0, |s| s.consecutive_failures),
            mailboxes,
        });
    }

    Ok(LocalDiagnostics { accounts })
}

/// The local diagnostics as JSON (the FFI/Swift-facing entry point).
/// MUST NEVER be written to the exported support bundle.
pub fn local_diagnostics_json(db: &Database) -> Result<String, CoreError> {
    let diag = local_diagnostics(db)?;
    serde_json::to_string(&diag)
        .map_err(|e| CoreError::Db(format!("serialize local diagnostics: {e}")))
}

/// The read-only integrity check as JSON.
pub fn integrity_check_json(db: &Database) -> Result<String, CoreError> {
    let r = db.quick_check().map_err(db_err)?;
    let report = IntegrityReport {
        ok: r.ok,
        issue_count: r.messages.len(),
        first_issue: r
            .messages
            .into_iter()
            .next()
            .map(|m| m.chars().take(200).collect()),
    };
    serde_json::to_string(&report)
        .map_err(|e| CoreError::Db(format!("serialize integrity report: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use castellum_models::{Account, Address, DraftAddress, Mailbox, NewDraft, NewMessage};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_db() -> Database {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-bundle-test-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        Database::open(&path).expect("open db")
    }

    // Sentinels planted in every dangerous channel. None may ever appear in
    // the bundle JSON or its Debug rendering.
    const EMAIL_SENTINEL: &str = "alice.secret.SENTINEL@example.test";
    const ACCOUNT_ID_SENTINEL: &str = "acct-id-SENTINEL";
    const FOLDER_SENTINEL: &str = "Clients/TopSecretDeal-SENTINEL";
    const SUBJECT_SENTINEL: &str = "SECRET-SUBJECT-SENTINEL";
    const SNIPPET_SENTINEL: &str = "secret-snippet-SENTINEL";
    const BODY_SENTINEL: &str = "secret-body-text-SENTINEL";
    const DRAFT_SENTINEL: &str = "secret-draft-body-SENTINEL";
    const OP_ERROR_SENTINEL: &str = "NO [AUTHENTICATIONFAILED] pass=hunter2-SENTINEL";
    const HOSTILE_ROW_SENTINEL: &str = "raw server error with user@host SENTINEL";
    const HOSTILE_CAP_SENTINEL: &str = "cap with spaces SENTINEL";
    // Hostile capabilities built ONLY from the charset a permissive
    // atom filter would accept — these must still never be echoed.
    const HOSTILE_CAP_IP_SENTINEL: &str = "X-SOURCE-IP-203.0.113.4-SENTINEL";
    const HOSTILE_CAP_ID_SENTINEL: &str = "ACCOUNT-ALICE-SECRET-SENTINEL";
    const UID_SENTINEL: i64 = 987_654_321;

    /// Seed a profile whose every risky field carries a sentinel, including
    /// hostile rows written directly to the diagnostic tables (simulating a
    /// legacy or tampered database — the allowlist must still hold).
    fn db_with_sentinels() -> Database {
        let db = temp_db();
        db.upsert_account(&Account {
            id: ACCOUNT_ID_SENTINEL.into(),
            email: EMAIL_SENTINEL.into(),
            display_name: Some("Alice SENTINEL".into()),
            provider: "imap".into(),
        })
        .unwrap();
        // This fixture enqueues a mark_read op to exercise diagnostics; needs writes.
        db.set_account_write_enabled(ACCOUNT_ID_SENTINEL, true)
            .unwrap();

        let mailbox_id = format!("{ACCOUNT_ID_SENTINEL}:{FOLDER_SENTINEL}");
        db.upsert_mailbox(&Mailbox {
            id: mailbox_id.clone(),
            account_id: ACCOUNT_ID_SENTINEL.into(),
            path: FOLDER_SENTINEL.into(),
            name: FOLDER_SENTINEL.into(),
            role: None,
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();

        let message_id = format!("{mailbox_id}:1");
        db.upsert_message(&NewMessage {
            id: message_id.clone(),
            account_id: ACCOUNT_ID_SENTINEL.into(),
            mailbox_id: mailbox_id.clone(),
            provider_uid: Some(1),
            rfc822_message_id: Some(format!("<{SUBJECT_SENTINEL}@x>")),
            subject: Some(SUBJECT_SENTINEL.into()),
            from_name: Some("Alice SENTINEL".into()),
            from_address: Some(EMAIL_SENTINEL.into()),
            sent_at: Some(1_700_000_000),
            snippet: Some(SNIPPET_SENTINEL.into()),
            size_bytes: Some(1234),
            has_attachments: false,
            attachments: Some(Vec::new()),
            is_read: false,
            is_starred: false,
            addresses: vec![Address {
                kind: "from".into(),
                name: Some("Alice SENTINEL".into()),
                address: EMAIL_SENTINEL.into(),
            }],
            text_body: Some(BODY_SENTINEL.into()),
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: Vec::new(),
        })
        .unwrap();

        db.save_draft(&NewDraft {
            id: None,
            account_id: ACCOUNT_ID_SENTINEL.into(),
            to: vec![DraftAddress {
                name: None,
                email: EMAIL_SENTINEL.into(),
            }],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: SUBJECT_SENTINEL.into(),
            body_text: DRAFT_SENTINEL.into(),
            body_html: None,
            from_email: None,
            from_name: None,
            in_reply_to: None,
            references_header: None,
        })
        .unwrap();

        // A failed operation whose last_error carries raw server text.
        db.mark_read(&message_id, true).unwrap();
        let ops = db.pending_operations(ACCOUNT_ID_SENTINEL).unwrap();
        assert!(!ops.is_empty());
        db.set_operation_status(ops[0].id, "failed", Some(OP_ERROR_SENTINEL))
            .unwrap();

        // Hostile rows written straight into the diagnostic tables: raw text
        // in the redacted columns, an unknown phase, a hostile capability.
        db.record_account_sync_failure(
            ACCOUNT_ID_SENTINEL,
            "auth",
            HOSTILE_ROW_SENTINEL,
            HOSTILE_ROW_SENTINEL,
        )
        .unwrap();
        db.record_mailbox_sync_failure(
            ACCOUNT_ID_SENTINEL,
            &mailbox_id,
            HOSTILE_ROW_SENTINEL,
            HOSTILE_ROW_SENTINEL,
            "select",
        )
        .unwrap();
        // A message the sync could not decode (decode-skip retry set): the
        // bundle may count it, never name its UID.
        db.record_message_decode_skip(
            ACCOUNT_ID_SENTINEL,
            &mailbox_id,
            UID_SENTINEL,
            1,
            "message could not be decoded and was skipped",
            0,
        )
        .unwrap();
        db.set_imap_capabilities(
            ACCOUNT_ID_SENTINEL,
            &serde_json::to_string(&vec![
                "IMAP4rev1".to_string(),
                HOSTILE_CAP_SENTINEL.to_string(),
                HOSTILE_CAP_IP_SENTINEL.to_string(),
                HOSTILE_CAP_ID_SENTINEL.to_string(),
            ])
            .unwrap(),
        )
        .unwrap();

        db
    }

    const ALL_SENTINELS: &[&str] = &[
        EMAIL_SENTINEL,
        ACCOUNT_ID_SENTINEL,
        FOLDER_SENTINEL,
        SUBJECT_SENTINEL,
        SNIPPET_SENTINEL,
        BODY_SENTINEL,
        DRAFT_SENTINEL,
        OP_ERROR_SENTINEL,
        HOSTILE_ROW_SENTINEL,
        HOSTILE_CAP_SENTINEL,
        HOSTILE_CAP_IP_SENTINEL,
        HOSTILE_CAP_ID_SENTINEL,
        // Substrings that would indicate partial leaks too:
        "SENTINEL",
        "hunter2",
        "203.0.113.4",
        "ALICE",
        "987654321",
    ];

    #[test]
    fn bundle_never_contains_sentinel_data_in_json_or_debug() {
        let db = db_with_sentinels();
        let bundle = generate_support_bundle(&db).expect("bundle");
        let json = serde_json::to_string(&bundle).unwrap();
        let debug = format!("{bundle:?}");
        for s in ALL_SENTINELS {
            assert!(!json.contains(s), "sentinel {s:?} leaked into JSON: {json}");
            assert!(
                !debug.contains(s),
                "sentinel {s:?} leaked into Debug: {debug}"
            );
        }
    }

    #[test]
    fn bundle_reports_redacted_state_and_safe_counts() {
        let db = db_with_sentinels();
        let bundle = generate_support_bundle(&db).expect("bundle");

        assert_eq!(bundle.bundle_schema, 1);
        assert!(bundle.integrity.ok);
        assert_eq!(
            bundle.integrity.first_issue, None,
            "the exported bundle must carry integrity counts only"
        );
        assert_eq!(bundle.schema_version, castellum_db::latest_schema_version());
        assert_eq!(bundle.accounts.len(), 1);

        let acct = &bundle.accounts[0];
        assert_eq!(acct.label, "account-1");
        assert_eq!(acct.provider, "imap");
        // Known kind survives; hostile phrase/phase are replaced, not echoed.
        assert_eq!(acct.last_error_kind.as_deref(), Some("auth"));
        assert_eq!(acct.last_error.as_deref(), Some(REDACTED));
        assert_eq!(acct.last_error_phase.as_deref(), Some(REDACTED));
        assert_eq!(acct.consecutive_failures, 1);
        // Hostile capabilities (including charset-permitted atoms) collapse
        // into a pure count; only the allowlisted canonical atom survives.
        assert_eq!(
            acct.capabilities,
            vec!["IMAP4rev1".to_string(), "OTHER(3)".to_string()]
        );
        assert_eq!(acct.operations.failed, 1);

        assert_eq!(acct.mailboxes.len(), 1);
        let mb = &acct.mailboxes[0];
        assert_eq!(mb.label, "folder-1", "non-role folders get ordinal labels");
        assert_eq!(mb.role, None);
        assert_eq!(mb.last_error_kind.as_deref(), Some(REDACTED));
        assert_eq!(mb.last_error.as_deref(), Some(REDACTED));
        assert_eq!(mb.last_error_phase.as_deref(), Some("select"));
        assert_eq!(mb.total_messages, 1);
        assert_eq!(mb.messages_with_body, 1);
        assert_eq!(mb.messages_without_body, 0);
        assert_eq!(
            mb.decode_skips_pending, 1,
            "the decode-skip backlog is exported as a count"
        );

        let local = local_diagnostics(&db).expect("local diagnostics");
        assert_eq!(local.accounts[0].mailboxes[0].decode_skips_pending, 1);
    }

    #[test]
    fn bundle_uses_role_names_for_well_known_roles() {
        let db = temp_db();
        db.upsert_account(&Account {
            id: "a1".into(),
            email: "u@x.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        for (path, role) in [("INBOX", Some("inbox")), ("Custom/Path", None)] {
            db.upsert_mailbox(&Mailbox {
                id: format!("a1:{path}"),
                account_id: "a1".into(),
                path: path.into(),
                name: path.into(),
                role: role.map(str::to_string),
                delimiter: None,
                subscribed: true,
                selectable: true,
                sync_by_default: true,
            })
            .unwrap();
        }
        let bundle = generate_support_bundle(&db).unwrap();
        let labels: Vec<&str> = bundle.accounts[0]
            .mailboxes
            .iter()
            .map(|m| m.label.as_str())
            .collect();
        assert!(labels.contains(&"inbox"));
        assert!(labels.contains(&"folder-1"));
        assert!(!serde_json::to_string(&bundle).unwrap().contains("Custom"));
    }

    #[test]
    fn integrity_check_json_is_ok_on_fresh_profile() {
        let db = temp_db();
        let json = integrity_check_json(&db).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["issue_count"], 0);
        assert!(v["first_issue"].is_null());
    }

    #[test]
    fn bundle_generation_is_read_only() {
        let db = db_with_sentinels();
        let before = dump_diagnostic_tables(&db);
        let _ = generate_support_bundle_json(&db).unwrap();
        let _ = integrity_check_json(&db).unwrap();
        assert_eq!(before, dump_diagnostic_tables(&db));
    }

    /// Cheap change detector over the tables the bundle reads.
    fn dump_diagnostic_tables(db: &Database) -> String {
        let acct = db.get_account_sync_state(ACCOUNT_ID_SENTINEL).unwrap();
        let mbs = db.list_mailbox_sync_states(ACCOUNT_ID_SENTINEL).unwrap();
        let ops = db.operation_status_counts(ACCOUNT_ID_SENTINEL).unwrap();
        let caps = db.get_imap_capabilities(ACCOUNT_ID_SENTINEL).unwrap();
        format!("{acct:?}|{mbs:?}|{ops:?}|{caps:?}")
    }

    #[test]
    fn timestamps_with_unexpected_shapes_are_dropped() {
        assert_eq!(
            allow_timestamp(Some("2026-07-10 12:00:00".into())).as_deref(),
            Some("2026-07-10 12:00:00")
        );
        assert_eq!(
            allow_timestamp(Some("evil@example.test 12:00".into())),
            None
        );
        assert_eq!(allow_timestamp(Some("2026-07-10T12:00:00Z".into())), None);
        assert_eq!(allow_timestamp(None), None);
    }

    // ── local_diagnostics tests ─────────────────────────────────────────────

    /// Local diagnostics includes the real email; the support bundle does NOT —
    /// verified from the SAME db in one test.
    #[test]
    fn local_diagnostics_includes_email_while_bundle_does_not() {
        const EMAIL: &str = "alice@example.test";
        const ACCOUNT_ID: &str = "acct-local-test";

        let db = temp_db();
        db.upsert_account(&Account {
            id: ACCOUNT_ID.into(),
            email: EMAIL.into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();

        let local = local_diagnostics(&db).unwrap();
        assert_eq!(local.accounts.len(), 1);
        assert_eq!(local.accounts[0].email, EMAIL);

        let bundle_json = generate_support_bundle_json(&db).unwrap();
        assert!(
            !bundle_json.contains(EMAIL),
            "bundle must not contain real email"
        );

        // The local JSON does contain the email (this is correct — it is
        // local-only and never exported).
        let local_json = local_diagnostics_json(&db).unwrap();
        assert!(
            local_json.contains(EMAIL),
            "local JSON must carry real email"
        );
    }

    /// An account with no sync_state rows appears as never-synced (all None /
    /// zero failures) rather than being absent.
    #[test]
    fn local_diagnostics_account_with_no_sync_state_appears_as_never_synced() {
        let db = temp_db();
        db.upsert_account(&Account {
            id: "acct-ns".into(),
            email: "never@example.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();

        let local = local_diagnostics(&db).unwrap();
        assert_eq!(local.accounts.len(), 1);
        let acct = &local.accounts[0];
        assert!(acct.last_attempt_at.is_none());
        assert!(acct.last_success_at.is_none());
        assert!(acct.last_error_kind.is_none());
        assert_eq!(acct.consecutive_failures, 0);
    }

    /// Hostile persisted kind/phase values are replaced with `"redacted"` just
    /// as they are in the bundle — the re-validation fires in local_diagnostics
    /// too.
    #[test]
    fn local_diagnostics_hostile_kind_and_phase_become_redacted() {
        let db = temp_db();
        let account_id = "acct-hostile";
        db.upsert_account(&Account {
            id: account_id.into(),
            email: "hostile@example.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();

        // Write a known kind ("auth") and an unknown phase ("INVALID_PHASE")
        // directly so we exercise the allowlist path.
        db.record_account_sync_failure(
            account_id,
            "auth",
            "authentication failed",
            "INVALID_PHASE_SENTINEL",
        )
        .unwrap();

        let local = local_diagnostics(&db).unwrap();
        let acct = &local.accounts[0];
        // Known kind survives.
        assert_eq!(acct.last_error_kind.as_deref(), Some("auth"));
        // Unknown phase is replaced.
        assert_eq!(acct.last_error_phase.as_deref(), Some(REDACTED));
    }

    /// local_diagnostics rows round-trip through the serialiser (no panics,
    /// valid JSON, expected field names present).
    #[test]
    fn local_diagnostics_round_trips_through_json() {
        let db = db_with_sentinels();
        let json = local_diagnostics_json(&db).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v["accounts"].is_array());
        let acct = &v["accounts"][0];
        assert!(acct["email"].is_string());
        assert!(acct["account_id"].is_string());
        assert!(acct["mailboxes"].is_array());
        let mb = &acct["mailboxes"][0];
        assert!(mb["path"].is_string());
    }

    /// local_diagnostics is read-only: the diagnostic tables are unchanged after
    /// the call.
    #[test]
    fn local_diagnostics_is_read_only() {
        let db = db_with_sentinels();
        let before = dump_diagnostic_tables(&db);
        let _ = local_diagnostics_json(&db).unwrap();
        assert_eq!(before, dump_diagnostic_tables(&db));
    }
}
