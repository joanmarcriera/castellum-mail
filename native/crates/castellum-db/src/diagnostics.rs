// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//! Durable sync diagnostics (migration 0016) and the read-only integrity
//! check. Written by the sync engine at its existing attempt/success/failure
//! checkpoints; read by support-bundle generation, which never writes.
//!
//! The error columns hold ONLY redaction-safe values chosen by the caller
//! (castellum-core's redact module): a fixed kind tag, a fixed phrase, and a
//! fixed phase name. This layer stores what it is given — the redaction
//! guarantee lives in core, and the bundle re-validates on read as
//! defence-in-depth.

use crate::{Database, DbError};

/// One account's durable sync health row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSyncState {
    pub account_id: String,
    pub last_attempt_at: Option<String>,
    pub last_success_at: Option<String>,
    pub last_error_kind: Option<String>,
    pub last_error_redacted: Option<String>,
    pub last_error_phase: Option<String>,
    pub consecutive_failures: i64,
}

/// One mailbox's durable sync health row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxSyncState {
    pub account_id: String,
    pub mailbox_id: String,
    pub last_attempt_at: Option<String>,
    pub last_success_at: Option<String>,
    pub last_error_kind: Option<String>,
    pub last_error_redacted: Option<String>,
    pub last_error_phase: Option<String>,
    pub consecutive_failures: i64,
}

/// Safe per-mailbox aggregate counts for the support bundle (no content).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxBodyCounts {
    pub mailbox_id: String,
    pub path: String,
    pub role: Option<String>,
    pub total_messages: i64,
    pub messages_with_body: i64,
    pub has_sync_cursor: bool,
    /// UIDs currently in the mailbox's decode-skip retry set (migration
    /// 0040): messages a sync could not decode and will retry on a backoff.
    /// A count only — never a UID or any server text (CLAUDE.md gotcha 5).
    pub decode_skips_pending: i64,
}

/// Result of `PRAGMA quick_check` (read-only; never modifies the profile).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrityCheckResult {
    pub ok: bool,
    /// Issue messages, capped at 5 by the PRAGMA argument. Empty when ok.
    /// quick_check messages reference only schema object names and page
    /// numbers — never row content.
    pub messages: Vec<String>,
}

impl Database {
    pub fn record_account_sync_attempt(&self, account_id: &str) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO account_sync_state (account_id, last_attempt_at)
             VALUES (?1, datetime('now'))
             ON CONFLICT(account_id)
             DO UPDATE SET last_attempt_at = datetime('now')",
            rusqlite::params![account_id],
        )?;
        Ok(())
    }

    pub fn record_account_sync_success(&self, account_id: &str) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO account_sync_state (account_id, last_success_at)
             VALUES (?1, datetime('now'))
             ON CONFLICT(account_id)
             DO UPDATE SET last_success_at = datetime('now'),
                           last_error_kind = NULL,
                           last_error_redacted = NULL,
                           last_error_phase = NULL,
                           consecutive_failures = 0",
            rusqlite::params![account_id],
        )?;
        Ok(())
    }

    pub fn record_account_sync_failure(
        &self,
        account_id: &str,
        kind: &str,
        redacted_summary: &str,
        phase: &str,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO account_sync_state
                 (account_id, last_error_kind, last_error_redacted, last_error_phase,
                  consecutive_failures)
             VALUES (?1, ?2, ?3, ?4, 1)
             ON CONFLICT(account_id)
             DO UPDATE SET last_error_kind = ?2,
                           last_error_redacted = ?3,
                           last_error_phase = ?4,
                           consecutive_failures = consecutive_failures + 1",
            rusqlite::params![account_id, kind, redacted_summary, phase],
        )?;
        Ok(())
    }

    pub fn get_account_sync_state(
        &self,
        account_id: &str,
    ) -> Result<Option<AccountSyncState>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT account_id, last_attempt_at, last_success_at, last_error_kind,
                    last_error_redacted, last_error_phase, consecutive_failures
             FROM account_sync_state WHERE account_id = ?1",
        )?;
        let mut rows = stmt.query_map(rusqlite::params![account_id], |r| {
            Ok(AccountSyncState {
                account_id: r.get(0)?,
                last_attempt_at: r.get(1)?,
                last_success_at: r.get(2)?,
                last_error_kind: r.get(3)?,
                last_error_redacted: r.get(4)?,
                last_error_phase: r.get(5)?,
                consecutive_failures: r.get(6)?,
            })
        })?;
        rows.next().transpose().map_err(DbError::from)
    }

    pub fn record_mailbox_sync_attempt(
        &self,
        account_id: &str,
        mailbox_id: &str,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO mailbox_sync_state (account_id, mailbox_id, last_attempt_at)
             VALUES (?1, ?2, datetime('now'))
             ON CONFLICT(account_id, mailbox_id)
             DO UPDATE SET last_attempt_at = datetime('now')",
            rusqlite::params![account_id, mailbox_id],
        )?;
        Ok(())
    }

    pub fn record_mailbox_sync_success(
        &self,
        account_id: &str,
        mailbox_id: &str,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO mailbox_sync_state (account_id, mailbox_id, last_success_at)
             VALUES (?1, ?2, datetime('now'))
             ON CONFLICT(account_id, mailbox_id)
             DO UPDATE SET last_success_at = datetime('now'),
                           last_error_kind = NULL,
                           last_error_redacted = NULL,
                           last_error_phase = NULL,
                           consecutive_failures = 0",
            rusqlite::params![account_id, mailbox_id],
        )?;
        Ok(())
    }

    pub fn record_mailbox_sync_failure(
        &self,
        account_id: &str,
        mailbox_id: &str,
        kind: &str,
        redacted_summary: &str,
        phase: &str,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO mailbox_sync_state
                 (account_id, mailbox_id, last_error_kind, last_error_redacted,
                  last_error_phase, consecutive_failures)
             VALUES (?1, ?2, ?3, ?4, ?5, 1)
             ON CONFLICT(account_id, mailbox_id)
             DO UPDATE SET last_error_kind = ?3,
                           last_error_redacted = ?4,
                           last_error_phase = ?5,
                           consecutive_failures = consecutive_failures + 1",
            rusqlite::params![account_id, mailbox_id, kind, redacted_summary, phase],
        )?;
        Ok(())
    }

    pub fn list_mailbox_sync_states(
        &self,
        account_id: &str,
    ) -> Result<Vec<MailboxSyncState>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT account_id, mailbox_id, last_attempt_at, last_success_at,
                    last_error_kind, last_error_redacted, last_error_phase,
                    consecutive_failures
             FROM mailbox_sync_state WHERE account_id = ?1 ORDER BY mailbox_id",
        )?;
        let rows = stmt.query_map(rusqlite::params![account_id], |r| {
            Ok(MailboxSyncState {
                account_id: r.get(0)?,
                mailbox_id: r.get(1)?,
                last_attempt_at: r.get(2)?,
                last_success_at: r.get(3)?,
                last_error_kind: r.get(4)?,
                last_error_redacted: r.get(5)?,
                last_error_phase: r.get(6)?,
                consecutive_failures: r.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(DbError::from)
    }

    /// Safe per-mailbox aggregates for the bundle: message/body counts,
    /// cursor presence and the decode-skip backlog only — no subjects,
    /// snippets, addresses, or UIDs.
    pub fn list_mailbox_body_counts(
        &self,
        account_id: &str,
    ) -> Result<Vec<MailboxBodyCounts>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT mb.id, mb.path, mb.role,
                    (SELECT COUNT(*) FROM messages m WHERE m.mailbox_id = mb.id),
                    (SELECT COUNT(*) FROM messages m
                     JOIN message_bodies b ON b.message_id = m.id
                     WHERE m.mailbox_id = mb.id),
                    EXISTS (SELECT 1 FROM sync_cursors c
                            WHERE c.account_id = mb.account_id AND c.mailbox_id = mb.id),
                    (SELECT COUNT(*) FROM message_decode_skips s
                     WHERE s.account_id = mb.account_id AND s.mailbox_id = mb.id)
             FROM mailboxes mb WHERE mb.account_id = ?1 ORDER BY mb.path",
        )?;
        let rows = stmt.query_map(rusqlite::params![account_id], |r| {
            Ok(MailboxBodyCounts {
                mailbox_id: r.get(0)?,
                path: r.get(1)?,
                role: r.get(2)?,
                total_messages: r.get(3)?,
                messages_with_body: r.get(4)?,
                has_sync_cursor: r.get(5)?,
                decode_skips_pending: r.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(DbError::from)
    }

    /// Operation counts by status for one account (no payloads, no errors).
    /// Returns (status, count) pairs exactly as stored.
    pub fn operation_status_counts(&self, account_id: &str) -> Result<Vec<(String, i64)>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT status, COUNT(*) FROM operations
             WHERE account_id = ?1 GROUP BY status ORDER BY status",
        )?;
        let rows = stmt.query_map(rusqlite::params![account_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(DbError::from)
    }

    /// Store the (already sanitised) IMAP CAPABILITY tokens for an account as
    /// a JSON array in app_meta. Overwrites the previous capture.
    pub fn set_imap_capabilities(
        &self,
        account_id: &str,
        capabilities_json: &str,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO app_meta (key, value) VALUES ('imap_capabilities:' || ?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![account_id, capabilities_json],
        )?;
        Ok(())
    }

    pub fn get_imap_capabilities(&self, account_id: &str) -> Result<Option<String>, DbError> {
        let conn = self.lock_conn();
        let mut stmt =
            conn.prepare("SELECT value FROM app_meta WHERE key = 'imap_capabilities:' || ?1")?;
        let mut rows = stmt.query_map(rusqlite::params![account_id], |r| r.get::<_, String>(0))?;
        rows.next().transpose().map_err(DbError::from)
    }

    /// Stored mailbox paths the previous trustworthy LIST did not contain
    ///. Sync removes a folder only once it has been absent
    /// from two consecutive listings; this is the memory between the two.
    /// Kept in `app_meta` as a JSON array, so it needs no schema change and
    /// survives a relaunch. Unreadable or missing means "none".
    pub fn mailboxes_absent_from_last_listing(
        &self,
        account_id: &str,
    ) -> Result<Vec<String>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT value FROM app_meta WHERE key = 'mailboxes_absent_from_listing:' || ?1",
        )?;
        let mut rows = stmt.query_map(rusqlite::params![account_id], |r| r.get::<_, String>(0))?;
        let stored: Option<String> = rows.next().transpose()?;
        Ok(stored
            .and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok())
            .unwrap_or_default())
    }

    /// Replace the set read by [`Self::mailboxes_absent_from_last_listing`];
    /// an empty set removes the row.
    pub fn set_mailboxes_absent_from_last_listing(
        &self,
        account_id: &str,
        paths: &[String],
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        if paths.is_empty() {
            conn.execute(
                "DELETE FROM app_meta WHERE key = 'mailboxes_absent_from_listing:' || ?1",
                rusqlite::params![account_id],
            )?;
            return Ok(());
        }
        let json = serde_json::to_string(paths).map_err(|e| DbError(e.to_string()))?;
        conn.execute(
            "INSERT INTO app_meta (key, value)
             VALUES ('mailboxes_absent_from_listing:' || ?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![account_id, json],
        )?;
        Ok(())
    }

    /// SQLite's UTC clock (`datetime('now')`), matching the timestamp format
    /// used by the diagnostic tables.
    pub fn now_utc(&self) -> Result<String, DbError> {
        let conn = self.lock_conn();
        let now: String = conn.query_row("SELECT datetime('now')", [], |r| r.get(0))?;
        Ok(now)
    }

    /// `PRAGMA quick_check(5)`: structural B-tree/page validation, read-only
    /// by SQLite semantics (it never writes, even in WAL mode). quick_check
    /// is chosen over integrity_check because it skips slow index-consistency
    /// scans yet still detects crash/filesystem corruption.
    pub fn quick_check(&self) -> Result<IntegrityCheckResult, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare("PRAGMA quick_check(5)")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let messages = rows.collect::<Result<Vec<_>, _>>()?;
        let ok = messages.len() == 1 && messages[0] == "ok";
        Ok(IntegrityCheckResult {
            ok,
            messages: if ok { Vec::new() } else { messages },
        })
    }
}

#[cfg(test)]
mod tests {
    use castellum_models::{Account, Mailbox};

    use crate::Database;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_db() -> Database {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-diagnostics-test-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        Database::open(&path).expect("open db")
    }

    fn seed_mailbox(db: &Database) -> (String, String) {
        let account_id = "acct-1".to_string();
        db.upsert_account(&Account {
            id: account_id.clone(),
            email: "user@example.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        let mailbox_id = format!("{account_id}:INBOX");
        db.upsert_mailbox(&Mailbox {
            id: mailbox_id.clone(),
            account_id: account_id.clone(),
            path: "INBOX".into(),
            name: "INBOX".into(),
            role: Some("inbox".into()),
            delimiter: None,
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        (account_id, mailbox_id)
    }

    #[test]
    fn mailbox_body_counts_include_the_pending_decode_skip_count() {
        // Review of PR #180, finding 3: the retry-set backlog is visible in
        // diagnostics as a COUNT only — never a UID or any server text.
        let db = temp_db();
        let (account_id, mailbox_id) = seed_mailbox(&db);
        let counts = db.list_mailbox_body_counts(&account_id).unwrap();
        assert_eq!(counts[0].decode_skips_pending, 0);
        for uid in [4, 9] {
            db.record_message_decode_skip(
                &account_id,
                &mailbox_id,
                uid,
                1,
                "message could not be decoded and was skipped",
                0,
            )
            .unwrap();
        }
        let counts = db.list_mailbox_body_counts(&account_id).unwrap();
        assert_eq!(counts[0].decode_skips_pending, 2);
    }

    #[test]
    fn account_sync_state_records_attempt_failure_and_success() {
        let db = temp_db();
        // No accounts row needed: connect failures precede account creation.
        db.record_account_sync_attempt("acct-x").unwrap();
        db.record_account_sync_failure("acct-x", "auth", "authentication failed", "connect")
            .unwrap();
        db.record_account_sync_failure("acct-x", "auth", "authentication failed", "connect")
            .unwrap();

        let s = db.get_account_sync_state("acct-x").unwrap().unwrap();
        assert!(s.last_attempt_at.is_some());
        assert!(s.last_success_at.is_none());
        assert_eq!(s.last_error_kind.as_deref(), Some("auth"));
        assert_eq!(s.last_error_phase.as_deref(), Some("connect"));
        assert_eq!(s.consecutive_failures, 2);

        db.record_account_sync_success("acct-x").unwrap();
        let s = db.get_account_sync_state("acct-x").unwrap().unwrap();
        assert!(s.last_success_at.is_some());
        assert_eq!(s.last_error_kind, None);
        assert_eq!(s.last_error_redacted, None);
        assert_eq!(s.last_error_phase, None);
        assert_eq!(s.consecutive_failures, 0);
    }

    #[test]
    fn mailbox_sync_state_failure_increments_and_success_resets() {
        let db = temp_db();
        let (account_id, mailbox_id) = seed_mailbox(&db);

        db.record_mailbox_sync_attempt(&account_id, &mailbox_id)
            .unwrap();
        db.record_mailbox_sync_failure(
            &account_id,
            &mailbox_id,
            "network",
            "network connection failed",
            "select",
        )
        .unwrap();
        db.record_mailbox_sync_failure(
            &account_id,
            &mailbox_id,
            "protocol",
            "IMAP protocol error",
            "fetch",
        )
        .unwrap();

        let states = db.list_mailbox_sync_states(&account_id).unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].consecutive_failures, 2);
        assert_eq!(states[0].last_error_kind.as_deref(), Some("protocol"));
        assert_eq!(states[0].last_error_phase.as_deref(), Some("fetch"));
        assert!(states[0].last_attempt_at.is_some());

        db.record_mailbox_sync_success(&account_id, &mailbox_id)
            .unwrap();
        let states = db.list_mailbox_sync_states(&account_id).unwrap();
        assert_eq!(states[0].consecutive_failures, 0);
        assert_eq!(states[0].last_error_kind, None);
        assert!(states[0].last_success_at.is_some());
    }

    #[test]
    fn mailbox_sync_state_cascades_on_account_delete() {
        let db = temp_db();
        let (account_id, mailbox_id) = seed_mailbox(&db);
        db.record_mailbox_sync_attempt(&account_id, &mailbox_id)
            .unwrap();

        {
            let conn = db.lock_conn();
            conn.execute(
                "DELETE FROM accounts WHERE id = ?1",
                rusqlite::params![account_id],
            )
            .unwrap();
        }
        assert!(db.list_mailbox_sync_states(&account_id).unwrap().is_empty());
    }

    #[test]
    fn imap_capabilities_round_trip() {
        let db = temp_db();
        assert_eq!(db.get_imap_capabilities("a").unwrap(), None);
        db.set_imap_capabilities("a", r#"["IMAP4rev1","UIDPLUS"]"#)
            .unwrap();
        db.set_imap_capabilities("a", r#"["IMAP4rev1","MOVE"]"#)
            .unwrap();
        assert_eq!(
            db.get_imap_capabilities("a").unwrap().as_deref(),
            Some(r#"["IMAP4rev1","MOVE"]"#)
        );
    }

    #[test]
    fn quick_check_on_fresh_db_returns_ok_with_no_messages() {
        let db = temp_db();
        let r = db.quick_check().unwrap();
        assert!(r.ok);
        assert!(r.messages.is_empty());
    }
}
