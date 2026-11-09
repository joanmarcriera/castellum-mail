// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//! Thread extraction rows (migration 0032, S54) — action items, dates and
//! amounts pulled from a thread by the `extract_thread_items` AI feature.
//!
//! Every row here was already validated against a fixed JSON schema in
//! `castellum-core::ai::defense::validate_output` /
//! `castellum-core::ai::extraction::parse_and_validate` before this module
//! ever sees it — this layer trusts its caller the same way `ai.rs` trusts
//! `run.rs` to have called `validate_output` first. `structured_value` is
//! stored as an opaque JSON string (already-validated shape) so the Swift
//! panel can decode it directly without this crate needing to know its
//! internal fields.
//!
//! Rows are replaced wholesale per thread (`replace_thread_extractions`):
//! there is no stable per-item identity across two extraction runs of the
//! same thread to reconcile against, so a superseded run's rows are deleted
//! before the fresh ones are inserted, in one transaction.

use rusqlite::params;

use crate::{Database, DbError};

/// One item to insert for a thread, as handed down by the executor after
/// schema validation. `structured_value` is `None` for an `action_item`, or
/// the item's pre-serialized (and already-validated) JSON object for a
/// `date`/`amount` item.
#[derive(Debug, Clone, PartialEq)]
pub struct NewThreadExtraction<'a> {
    pub message_id: Option<&'a str>,
    pub kind: &'a str,
    pub text: &'a str,
    pub structured_value: Option<&'a str>,
}

/// A stored row, as read back for the Swift panel.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadExtractionRecord {
    pub id: i64,
    pub thread_id: String,
    pub message_id: Option<String>,
    pub kind: String,
    pub text: String,
    /// Opaque JSON text (an object) or `None` — never parsed here.
    pub structured_value: Option<String>,
    pub created_at: String,
}

impl Database {
    /// Replace every `thread_extractions` row for `thread_id` with `items`,
    /// atomically (delete-then-insert in one transaction — a reader never
    /// observes a partially-replaced set).
    ///
    /// Called only after `defense::validate_output` accepted the model's
    /// response for `extract_thread_items`; `items` is never raw model text.
    pub fn replace_thread_extractions(
        &self,
        thread_id: &str,
        items: &[NewThreadExtraction<'_>],
    ) -> Result<(), DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM thread_extractions WHERE thread_id = ?1",
            params![thread_id],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO thread_extractions
                     (thread_id, message_id, kind, text, structured_value)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for item in items {
                stmt.execute(params![
                    thread_id,
                    item.message_id,
                    item.kind,
                    item.text,
                    item.structured_value,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Fetch every stored extraction for a thread, oldest first (insertion
    /// order within the one `replace_thread_extractions` call that wrote
    /// them). Returns an empty vec for a thread with no extraction yet —
    /// never an error; the Swift panel treats that as "nothing extracted".
    pub fn get_thread_extractions(
        &self,
        thread_id: &str,
    ) -> Result<Vec<ThreadExtractionRecord>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT id, thread_id, message_id, kind, text, structured_value, created_at
             FROM thread_extractions WHERE thread_id = ?1 ORDER BY id",
        )?;
        let rows = stmt
            .query_map(params![thread_id], |r| {
                Ok(ThreadExtractionRecord {
                    id: r.get(0)?,
                    thread_id: r.get(1)?,
                    message_id: r.get(2)?,
                    kind: r.get(3)?,
                    text: r.get(4)?,
                    structured_value: r.get(5)?,
                    created_at: r.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

/// Look up a single row by id — test-only convenience (no production caller
/// needs a single extraction by id; the panel always reads the whole thread).
#[cfg(test)]
impl Database {
    fn get_thread_extraction_by_id_for_test(
        &self,
        id: i64,
    ) -> Result<Option<ThreadExtractionRecord>, DbError> {
        use rusqlite::OptionalExtension;
        let conn = self.lock_conn();
        conn.query_row(
            "SELECT id, thread_id, message_id, kind, text, structured_value, created_at
             FROM thread_extractions WHERE id = ?1",
            params![id],
            |r| {
                Ok(ThreadExtractionRecord {
                    id: r.get(0)?,
                    thread_id: r.get(1)?,
                    message_id: r.get(2)?,
                    kind: r.get(3)?,
                    text: r.get(4)?,
                    structured_value: r.get(5)?,
                    created_at: r.get(6)?,
                })
            },
        )
        .optional()
        .map_err(DbError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use castellum_models::{Account, Mailbox, NewMessage};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn fresh_db() -> Database {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-thread-extractions-test-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        Database::open(&path).expect("open")
    }

    /// Seed one account/mailbox/message and re-derive threads, returning the
    /// thread id `rebuild_threads` assigns a standalone message (`t:<msg id>`
    /// — see `repo::rebuild_threads`'s doc comment on thread-root selection).
    fn seed_thread(db: &Database, uid: i64) -> String {
        db.upsert_account(&Account {
            id: "acc1".into(),
            email: "test@local.test".into(),
            display_name: Some("Test".into()),
            provider: "imap".into(),
        })
        .unwrap();
        db.upsert_mailbox(&Mailbox {
            id: "acc1:INBOX".into(),
            account_id: "acc1".into(),
            path: "INBOX".into(),
            name: "INBOX".into(),
            role: Some("inbox".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.set_account_write_enabled("acc1", true).unwrap();
        db.upsert_message(&NewMessage {
            id: format!("acc1:INBOX:{uid}"),
            account_id: "acc1".into(),
            mailbox_id: "acc1:INBOX".into(),
            provider_uid: Some(uid),
            rfc822_message_id: Some(format!("<{uid}@local.test>")),
            subject: Some("subj".into()),
            from_name: Some("Alice".into()),
            from_address: Some("alice@local.test".into()),
            sent_at: Some(1_700_000_000),
            snippet: Some("hello".into()),
            size_bytes: Some(1234),
            has_attachments: false,
            attachments: None,
            is_read: false,
            is_starred: false,
            addresses: vec![],
            text_body: Some("hello".into()),
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: vec![],
        })
        .unwrap();
        db.rebuild_threads("acc1").unwrap();
        format!("t:acc1:INBOX:{uid}")
    }

    #[test]
    fn replace_is_atomic_delete_then_insert() {
        let db = fresh_db();
        let thread_id = seed_thread(&db, 1);

        db.replace_thread_extractions(
            &thread_id,
            &[
                NewThreadExtraction {
                    message_id: None,
                    kind: "action_item",
                    text: "Reply to Jane by Friday",
                    structured_value: None,
                },
                NewThreadExtraction {
                    message_id: None,
                    kind: "date",
                    text: "Meeting on 2026-09-25",
                    structured_value: Some(r#"{"date":"2026-09-25"}"#),
                },
            ],
        )
        .unwrap();
        let first_pass = db.get_thread_extractions(&thread_id).unwrap();
        assert_eq!(first_pass.len(), 2);

        // A second run with fewer items must leave exactly the new set —
        // never accumulate alongside the first run's rows.
        db.replace_thread_extractions(
            &thread_id,
            &[NewThreadExtraction {
                message_id: None,
                kind: "amount",
                text: "Invoice total $1,250.00",
                structured_value: Some(r#"{"amount":1250.0,"currency":"USD"}"#),
            }],
        )
        .unwrap();
        let second_pass = db.get_thread_extractions(&thread_id).unwrap();
        assert_eq!(
            second_pass.len(),
            1,
            "stale rows from the first run must be gone"
        );
        assert_eq!(second_pass[0].kind, "amount");
        assert_eq!(
            second_pass[0].structured_value.as_deref(),
            Some(r#"{"amount":1250.0,"currency":"USD"}"#)
        );
    }

    #[test]
    fn get_thread_extractions_is_empty_not_error_for_unknown_thread() {
        let db = fresh_db();
        let rows = db.get_thread_extractions("t:nonexistent").unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn extraction_deleted_when_thread_is_deleted() {
        let db = fresh_db();
        let thread_id = seed_thread(&db, 2);
        db.replace_thread_extractions(
            &thread_id,
            &[NewThreadExtraction {
                message_id: None,
                kind: "action_item",
                text: "Do the thing",
                structured_value: None,
            }],
        )
        .unwrap();
        let rows = db.get_thread_extractions(&thread_id).unwrap();
        let id = rows[0].id;

        db.lock_conn()
            .execute("DELETE FROM threads WHERE id = ?1", params![thread_id])
            .unwrap();

        assert!(
            db.get_thread_extraction_by_id_for_test(id)
                .unwrap()
                .is_none(),
            "ON DELETE CASCADE must remove the extraction with its thread"
        );
    }
}
