// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//! Work-item provenance (migration 0050): an explicit "this work item came
//! from message X / suggestion Y / the user" record.
//!
//! Usage: `record_work_item_provenance` (manual attribution),
//! `list_work_item_provenance(work_item_id)`, `work_items_from_source`. The
//! suggestion-accept and message-link paths write rows through [`record_in`].
//!
//! Invariants: rows are append-only and idempotent (primary key
//! `(work_item_id, source_kind, source_id)`); `work_items.notes` is user-owned
//! and is NEVER read or written here; rows cascade away with the work item.

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::work_items::now_epoch;
use crate::{Database, DbError};

/// One provenance row. Serialized snake_case for the FFI/Swift boundary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkItemProvenance {
    pub work_item_id: String,
    /// `message` | `suggestion` | `manual`.
    pub source_kind: String,
    /// Message id, suggestion id, or `""` for `manual`.
    pub source_id: String,
    pub created_at: i64,
}

/// Idempotent insert on an already-open connection/transaction.
pub(crate) fn record_in(
    conn: &rusqlite::Connection,
    work_item_id: &str,
    source_kind: &str,
    source_id: &str,
    now: i64,
) -> Result<(), DbError> {
    conn.execute(
        "INSERT OR IGNORE INTO work_item_provenance
             (work_item_id, source_kind, source_id, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![work_item_id, source_kind, source_id, now],
    )?;
    Ok(())
}

impl Database {
    /// Record a manual "came from" attribution. `source_kind` must be
    /// `manual` (`source_id` is ignored, stored as `""`); `message` and
    /// `suggestion` rows are written only by the internal accept/link paths
    /// and are rejected here. The item must exist.
    /// Idempotent: a repeat returns the original row.
    pub fn record_work_item_provenance(
        &self,
        work_item_id: &str,
        source_kind: &str,
        source_id: &str,
    ) -> Result<WorkItemProvenance, DbError> {
        self.record_work_item_provenance_at(work_item_id, source_kind, source_id, now_epoch())
    }

    /// [`Database::record_work_item_provenance`] with an injected clock.
    pub fn record_work_item_provenance_at(
        &self,
        work_item_id: &str,
        source_kind: &str,
        source_id: &str,
        now: i64,
    ) -> Result<WorkItemProvenance, DbError> {
        // Message/suggestion rows are written only by the internal
        // accept/link paths (`record_in`); the public call is manual-only.
        if source_kind != "manual" {
            return Err(DbError(format!(
                "only source_kind \"manual\" may be recorded directly, got {source_kind:?}"
            )));
        }
        let _ = source_id; // manual ignores it; stored as ""
        let source_id = "";
        let conn = self.lock_conn();
        let exists = conn
            .query_row(
                "SELECT 1 FROM work_items WHERE id = ?1",
                params![work_item_id],
                |_| Ok(()),
            )
            .is_ok();
        if !exists {
            return Err(DbError(format!("work item {work_item_id} not found")));
        }
        record_in(&conn, work_item_id, source_kind, source_id, now)?;
        Ok(conn.query_row(
            "SELECT work_item_id, source_kind, source_id, created_at
             FROM work_item_provenance
             WHERE work_item_id = ?1 AND source_kind = ?2 AND source_id = ?3",
            params![work_item_id, source_kind, source_id],
            row,
        )?)
    }

    /// A work item's provenance, oldest first (ties by kind, id).
    pub fn list_work_item_provenance(
        &self,
        work_item_id: &str,
    ) -> Result<Vec<WorkItemProvenance>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT work_item_id, source_kind, source_id, created_at
             FROM work_item_provenance WHERE work_item_id = ?1
             ORDER BY created_at, source_kind, source_id",
        )?;
        let rows = stmt
            .query_map(params![work_item_id], row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Reverse lookup: work-item ids that came from this source, sorted.
    pub fn work_items_from_source(
        &self,
        source_kind: &str,
        source_id: &str,
    ) -> Result<Vec<String>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT work_item_id FROM work_item_provenance
             WHERE source_kind = ?1 AND source_id = ?2 ORDER BY work_item_id",
        )?;
        let rows = stmt
            .query_map(params![source_kind, source_id], |r| r.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(rows)
    }
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<WorkItemProvenance> {
    Ok(WorkItemProvenance {
        work_item_id: r.get(0)?,
        source_kind: r.get(1)?,
        source_id: r.get(2)?,
        created_at: r.get(3)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EntityKind, NewSuggestion, WorkItemUpsert};
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn fresh_db() -> Database {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-provenance-test-{}-{}.db",
            std::process::id(),
            n
        ));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
        let db = Database::open(&path).expect("open");
        db.lock_conn()
            .execute_batch(
                "INSERT INTO accounts (id, email, display_name, provider)
                     VALUES ('acc', 'me@local.test', 'Me', 'imap');
                 INSERT INTO mailboxes (id, account_id, path, name, role)
                     VALUES ('acc:INBOX', 'acc', 'INBOX', 'INBOX', 'inbox');
                 INSERT INTO messages (id, account_id, mailbox_id, provider_uid,
                                       rfc822_message_id, thread_id, from_address)
                     VALUES ('m1', 'acc', 'acc:INBOX', 1, '<1@x.test>', 't1', 'a@x.test');",
            )
            .expect("seed");
        db
    }

    fn item(db: &Database, notes: &str) -> String {
        db.upsert_work_item_at(
            WorkItemUpsert {
                id: None,
                project_id: None,
                title: "Task",
                notes,
                state: "needs_reply",
                position: 0,
                ..Default::default()
            },
            5,
        )
        .unwrap()
        .id
    }

    #[test]
    fn accepting_a_suggestion_records_suggestion_and_message_sources() {
        let db = fresh_db();
        let s = db
            .propose_suggestion_at(
                &NewSuggestion {
                    kind: "work_item",
                    payload_json: r#"{"title":"Call Jane"}"#,
                    message_id: Some("m1"),
                    thread_id: Some("t1"),
                    origin: "ai_extraction",
                    dedupe_key: "k",
                    ..Default::default()
                },
                10,
            )
            .unwrap()
            .unwrap();
        let w = db
            .accept_suggestion_at(&s.id, 20)
            .unwrap()
            .work_item
            .unwrap();
        let p = db.list_work_item_provenance(&w.id).unwrap();
        let got: Vec<_> = p
            .iter()
            .map(|r| (r.source_kind.as_str(), r.source_id.as_str(), r.created_at))
            .collect();
        assert_eq!(got, vec![("message", "m1", 20), ("suggestion", &*s.id, 20)]);
        // Idempotent accept adds nothing; notes stay the user's (empty).
        db.accept_suggestion_at(&s.id, 99).unwrap();
        assert_eq!(db.list_work_item_provenance(&w.id).unwrap().len(), 2);
        assert_eq!(w.notes, "");
        assert_eq!(
            db.work_items_from_source("message", "m1").unwrap(),
            vec![w.id]
        );
    }

    #[test]
    fn linking_a_message_records_provenance_but_never_touches_notes() {
        let db = fresh_db();
        let id = item(&db, "my own notes");
        db.link_work_item(&id, EntityKind::Message, "m1").unwrap();
        db.link_work_item(&id, EntityKind::Message, "m1").unwrap();
        db.link_work_item(&id, EntityKind::Thread, "t1").unwrap();
        let p = db.list_work_item_provenance(&id).unwrap();
        assert_eq!(p.len(), 1, "thread links and repeats add no rows");
        assert_eq!(
            (p[0].source_kind.as_str(), p[0].source_id.as_str()),
            ("message", "m1")
        );
        let notes = db
            .list_work_items_filtered(&Default::default())
            .unwrap()
            .into_iter()
            .find(|w| w.id == id)
            .unwrap()
            .notes;
        assert_eq!(notes, "my own notes");
    }

    #[test]
    fn manual_and_validation_and_cascade() {
        let db = fresh_db();
        let id = item(&db, "");
        let m = db
            .record_work_item_provenance_at(&id, "manual", "ignored", 7)
            .unwrap();
        assert_eq!(
            (m.source_kind.as_str(), m.source_id.as_str(), m.created_at),
            ("manual", "", 7)
        );
        let again = db
            .record_work_item_provenance_at(&id, "manual", "", 9)
            .unwrap();
        assert_eq!(again.created_at, 7, "repeat returns the original row");
        assert!(db.record_work_item_provenance(&id, "bogus", "x").is_err());
        assert!(db
            .record_work_item_provenance(&id, "message", "m1")
            .is_err());
        assert!(db
            .record_work_item_provenance(&id, "suggestion", "s1")
            .is_err());
        assert!(db
            .record_work_item_provenance("nope", "manual", "")
            .is_err());
        db.delete_work_item(&id).unwrap();
        assert!(db.list_work_item_provenance(&id).unwrap().is_empty());
    }
}
