// Castellum - contact enrichment persistence layer (slice 8d).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Stores external enrichment fields per (contact_id, source, field). The
// UNIQUE constraint enables upsert-by-key semantics: re-running an adapter
// replaces stale values for the same source+field pair. ON DELETE CASCADE
// keeps the table clean when a contact is removed.

use crate::{Database, DbError};
use rusqlite::params;
use serde::{Deserialize, Serialize};

/// One enrichment field row as returned by `list_enrichment`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrichmentRow {
    pub id: i64,
    pub contact_id: i64,
    pub source: String,
    pub field: String,
    pub value: String,
    pub fetched_at: i64,
    pub provenance: Option<String>,
}

impl Database {
    /// Upsert a batch of enrichment fields for `contact_id` from `source`.
    /// Each tuple is `(field, value, provenance)`. `fetched_at` applies to
    /// the whole batch. The whole batch runs in ONE transaction — either all
    /// fields land or none do.
    ///
    /// Semantics are ADDITIVE per (contact_id, source, field): an existing
    /// key is updated in place (value/fetched_at/provenance refreshed), a new
    /// key is inserted, and keys ABSENT from `fields` are left untouched.
    /// A re-paste that parses to fewer fields therefore does NOT remove
    /// previously saved fields — per-field `delete_enrichment` is the
    /// correction path.
    pub fn upsert_enrichment_fields(
        &self,
        contact_id: i64,
        source: &str,
        fields: &[(String, String, Option<String>)],
        fetched_at: i64,
    ) -> Result<(), DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO contact_enrichment (contact_id, source, field, value, fetched_at, provenance)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(contact_id, source, field) DO UPDATE SET
                     value      = excluded.value,
                     fetched_at = excluded.fetched_at,
                     provenance = excluded.provenance",
            )?;
            for (field, value, provenance) in fields {
                stmt.execute(params![
                    contact_id, source, field, value, fetched_at, provenance
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// All enrichment rows for `contact_id`, ordered by (source, field).
    pub fn list_enrichment(&self, contact_id: i64) -> Result<Vec<EnrichmentRow>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT id, contact_id, source, field, value, fetched_at, provenance
             FROM contact_enrichment
             WHERE contact_id = ?1
             ORDER BY source, field",
        )?;
        let rows = stmt.query_map(params![contact_id], |r| {
            Ok(EnrichmentRow {
                id: r.get(0)?,
                contact_id: r.get(1)?,
                source: r.get(2)?,
                field: r.get(3)?,
                value: r.get(4)?,
                fetched_at: r.get(5)?,
                provenance: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Delete a single enrichment row by id. Returns `DbError` if not found.
    pub fn delete_enrichment(&self, id: i64) -> Result<(), DbError> {
        let conn = self.lock_conn();
        let n = conn.execute("DELETE FROM contact_enrichment WHERE id = ?1", params![id])?;
        if n == 0 {
            return Err(DbError(format!("enrichment row {id} not found")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn fresh_db() -> Database {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-enrichment-test-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        Database::open(&path).expect("open")
    }

    /// Create a user-owned contact for testing and return its id.
    fn make_contact(db: &Database, email: &str) -> i64 {
        db.create_contact(email, None).expect("create contact").id
    }

    #[test]
    fn upsert_by_contact_source_field_replaces_value() {
        let db = fresh_db();
        let contact_id = make_contact(&db, "alice@x.test");
        let fields = vec![(
            "title".to_string(),
            "Engineer".to_string(),
            Some("pasted 2026-07-03".to_string()),
        )];
        db.upsert_enrichment_fields(contact_id, "manual", &fields, 1_000_000)
            .unwrap();
        let rows = db.list_enrichment(contact_id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value, "Engineer");
        assert_eq!(rows[0].source, "manual");
        assert_eq!(rows[0].field, "title");
        assert_eq!(rows[0].contact_id, contact_id);

        // Upsert again with a different value — must replace, not append.
        let fields2 = vec![(
            "title".to_string(),
            "Senior Engineer".to_string(),
            Some("pasted 2026-07-04".to_string()),
        )];
        db.upsert_enrichment_fields(contact_id, "manual", &fields2, 1_000_001)
            .unwrap();
        let rows2 = db.list_enrichment(contact_id).unwrap();
        assert_eq!(rows2.len(), 1, "no duplicate row created");
        assert_eq!(rows2[0].value, "Senior Engineer");
        assert_eq!(rows2[0].fetched_at, 1_000_001);
        assert_eq!(rows2[0].provenance.as_deref(), Some("pasted 2026-07-04"));
    }

    #[test]
    fn delete_field() {
        let db = fresh_db();
        let contact_id = make_contact(&db, "bob@x.test");
        let fields = vec![
            ("company".to_string(), "Acme".to_string(), None),
            ("title".to_string(), "CEO".to_string(), None),
        ];
        db.upsert_enrichment_fields(contact_id, "manual", &fields, 1_000_000)
            .unwrap();
        let rows = db.list_enrichment(contact_id).unwrap();
        assert_eq!(rows.len(), 2);

        let id_to_delete = rows.iter().find(|r| r.field == "company").unwrap().id;
        db.delete_enrichment(id_to_delete).unwrap();

        let after = db.list_enrichment(contact_id).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].field, "title");

        // Deleting a non-existent id must return an error.
        assert!(db.delete_enrichment(999_999).is_err());
    }

    #[test]
    fn cascade_on_contact_delete() {
        let db = fresh_db();
        let contact_id = make_contact(&db, "carol@x.test");
        let fields = vec![("summary".to_string(), "VP Engineering".to_string(), None)];
        db.upsert_enrichment_fields(contact_id, "manual", &fields, 1_000_000)
            .unwrap();
        assert!(!db.list_enrichment(contact_id).unwrap().is_empty());

        // Delete the contact — enrichment rows must cascade-delete.
        db.delete_contact_for_test(contact_id).unwrap();
        let rows = db.list_enrichment(contact_id).unwrap();
        assert!(rows.is_empty(), "cascade deleted enrichment rows");
    }

    #[test]
    fn ordering_by_source_then_field() {
        let db = fresh_db();
        let contact_id = make_contact(&db, "dave@x.test");
        // Insert in reverse alphabetical field order; confirm ORDER BY works.
        let fields = vec![
            ("title".to_string(), "T".to_string(), None),
            ("company".to_string(), "C".to_string(), None),
        ];
        db.upsert_enrichment_fields(contact_id, "manual", &fields, 1_000_000)
            .unwrap();
        let rows = db.list_enrichment(contact_id).unwrap();
        // Ordered by (source, field) → company < title alphabetically.
        assert_eq!(rows[0].field, "company");
        assert_eq!(rows[1].field, "title");
    }
}
