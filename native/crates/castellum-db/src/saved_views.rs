// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//! Saved Active Work views (migration 0047) — the AW3 backend.
//!
//! A view is a name plus a [`WorkItemFilter`]. There is exactly ONE filter
//! evaluator, [`crate::work_items::filter_clauses`], shared by
//! `list_work_items_filtered` (the rows of a view) and by
//! [`Database::saved_view_counts_tz`] (the badge numbers of every view in a
//! single statement). Filters are validated on every write: an unknown key,
//! an unknown state or an unknown due window is rejected and nothing is
//! stored. The stored text is the canonical re-serialisation of the parsed
//! filter, so it never carries keys the evaluator would ignore.
//!
//! Built-in views (Today, Needs reply, Waiting on others, Someday) are seeded
//! on first use with fixed `builtin:*` ids and `INSERT OR IGNORE`, so
//! seeding any number of times leaves exactly four rows. They can be
//! reordered and hidden, never deleted, renamed or re-filtered. Custom views
//! support create, rename, re-filter, hide, delete and reorder.
//!
//! Counts: one `SELECT SUM(CASE WHEN <view clause> ...) FROM work_items` with
//! every visible view as a column. That is a single pass over `work_items`
//! (the link sub-selects are uncorrelated, so SQLite evaluates each once),
//! not one query per view and not a per-row probe.

use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::work_items::{filter_clauses, local_utc_offset_secs, now_epoch};
use crate::{Database, DbError, WorkItemFilter};

/// Fixed id prefix of the seeded views.
pub const BUILTIN_VIEW_PREFIX: &str = "builtin:";

/// (id, name, filter JSON) of the seeded views, in display order.
const BUILTIN_VIEWS: [(&str, &str, &str); 4] = [
    (
        "builtin:today",
        "Today",
        r#"{"states":["needs_reply","waiting","delegated","blocked"],"due":["overdue","today"]}"#,
    ),
    (
        "builtin:needs_reply",
        "Needs reply",
        r#"{"waiting_on_me":true}"#,
    ),
    (
        "builtin:waiting_on_others",
        "Waiting on others",
        r#"{"waiting_on_others":true}"#,
    ),
    ("builtin:someday", "Someday", r#"{"states":["someday"]}"#),
];

/// A stored saved view. Serialized snake_case for the FFI/Swift boundary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedView {
    pub id: String,
    pub name: String,
    pub filter: WorkItemFilter,
    pub position: i64,
    pub built_in: bool,
    pub hidden: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One visible view's item count.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedViewCount {
    pub id: String,
    pub count: i64,
}

/// Parse and validate a filter written by a caller. Unknown keys, states and
/// due windows are errors; the filter must be a JSON object.
pub fn parse_view_filter(json: &str) -> Result<WorkItemFilter, DbError> {
    let f: WorkItemFilter = serde_json::from_str(json)
        .map_err(|e| DbError(format!("invalid saved-view filter: {e}")))?;
    // Building the clauses validates the states; the binds are discarded.
    filter_clauses(&f, 0, 0, &mut Vec::new())?;
    Ok(f)
}

fn clean_name(name: &str) -> Result<String, DbError> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > 120 {
        return Err(DbError("saved-view name must be 1-120 characters".into()));
    }
    Ok(n.to_string())
}

const VIEW_COLUMNS: &str = "id, name, filter, position, built_in, hidden, created_at, updated_at";

fn row_to_view(r: &rusqlite::Row<'_>) -> rusqlite::Result<SavedView> {
    let filter_text: String = r.get(2)?;
    // Stored text was validated on write; fall back to "match everything"
    // rather than failing the whole list if a row was hand-edited.
    let filter = serde_json::from_str(&filter_text).unwrap_or_default();
    Ok(SavedView {
        id: r.get(0)?,
        name: r.get(1)?,
        filter,
        position: r.get(3)?,
        built_in: r.get::<_, i64>(4)? != 0,
        hidden: r.get::<_, i64>(5)? != 0,
        created_at: r.get(6)?,
        updated_at: r.get(7)?,
    })
}

/// The counts statement for `views` (visible ones, in order) and its binds.
fn counts_sql(
    views: &[SavedView],
    now: i64,
    tz: i64,
) -> Result<Option<(String, Vec<Value>)>, DbError> {
    if views.is_empty() {
        return Ok(None);
    }
    let mut binds = Vec::new();
    let mut cols = Vec::new();
    for v in views {
        let clauses = filter_clauses(&v.filter, now, tz, &mut binds)?;
        let cond = if clauses.is_empty() {
            "1".to_string()
        } else {
            clauses.join(" AND ")
        };
        cols.push(format!(
            "COALESCE(SUM(CASE WHEN {cond} THEN 1 ELSE 0 END), 0)"
        ));
    }
    Ok(Some((
        format!("SELECT {} FROM work_items", cols.join(", ")),
        binds,
    )))
}

impl Database {
    /// Insert the four built-in views if they are missing. Idempotent: rows
    /// that exist (even hidden or reordered) are left exactly as they are.
    pub fn seed_builtin_saved_views(&self) -> Result<(), DbError> {
        self.seed_builtin_saved_views_at(now_epoch())
    }

    /// [`Database::seed_builtin_saved_views`] with an injected clock.
    pub fn seed_builtin_saved_views_at(&self, now: i64) -> Result<(), DbError> {
        let mut conn = self.lock_conn();
        let present: i64 = conn.query_row(
            "SELECT COUNT(*) FROM saved_views WHERE built_in = 1",
            [],
            |r| r.get(0),
        )?;
        if present as usize >= BUILTIN_VIEWS.len() {
            return Ok(());
        }
        let tx = conn.transaction()?;
        for (id, name, filter) in BUILTIN_VIEWS {
            let next: i64 = tx.query_row(
                "SELECT COALESCE(MAX(position) + 1, 0) FROM saved_views",
                [],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO saved_views
                     (id, name, filter, position, built_in, hidden, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 1, 0, ?5, ?5)",
                params![id, name, filter, next, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Saved views in display order (position, id). Hidden ones only when
    /// `include_hidden`. Seeds the built-ins on first use.
    pub fn list_saved_views(&self, include_hidden: bool) -> Result<Vec<SavedView>, DbError> {
        self.seed_builtin_saved_views()?;
        let conn = self.lock_conn();
        let sql = format!(
            "SELECT {VIEW_COLUMNS} FROM saved_views {} ORDER BY position, id",
            if include_hidden {
                ""
            } else {
                "WHERE hidden = 0"
            }
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_view)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// One saved view by id.
    pub fn get_saved_view(&self, id: &str) -> Result<Option<SavedView>, DbError> {
        let conn = self.lock_conn();
        Ok(conn
            .query_row(
                &format!("SELECT {VIEW_COLUMNS} FROM saved_views WHERE id = ?1"),
                params![id],
                row_to_view,
            )
            .optional()?)
    }

    /// Create a custom view at the end of the list. The filter is validated
    /// and canonicalised; names are unique case-insensitively.
    pub fn create_saved_view(&self, name: &str, filter_json: &str) -> Result<SavedView, DbError> {
        self.create_saved_view_at(name, filter_json, now_epoch())
    }

    /// [`Database::create_saved_view`] with an injected clock.
    pub fn create_saved_view_at(
        &self,
        name: &str,
        filter_json: &str,
        now: i64,
    ) -> Result<SavedView, DbError> {
        let name = clean_name(name)?;
        let filter = parse_view_filter(filter_json)?;
        self.seed_builtin_saved_views_at(now)?;
        let id = Uuid::new_v4().to_string();
        {
            let conn = self.lock_conn();
            let dup: i64 = conn.query_row(
                "SELECT COUNT(*) FROM saved_views WHERE lower(name) = lower(?1)",
                params![name],
                |r| r.get(0),
            )?;
            if dup > 0 {
                return Err(DbError(format!(
                    "a saved view named {name:?} already exists"
                )));
            }
            conn.execute(
                "INSERT INTO saved_views
                     (id, name, filter, position, built_in, hidden, created_at, updated_at)
                 VALUES (?1, ?2, ?3,
                         (SELECT COALESCE(MAX(position) + 1, 0) FROM saved_views),
                         0, 0, ?4, ?4)",
                params![
                    id,
                    name,
                    serde_json::to_string(&filter).unwrap_or_default(),
                    now
                ],
            )?;
        }
        self.get_saved_view(&id)?
            .ok_or_else(|| DbError("saved view vanished after insert".into()))
    }

    /// Update a view: any of `name`, `filter_json`, `hidden`. A built-in
    /// accepts only `hidden`; asking to rename or re-filter it is an error.
    pub fn update_saved_view(
        &self,
        id: &str,
        name: Option<&str>,
        filter_json: Option<&str>,
        hidden: Option<bool>,
    ) -> Result<SavedView, DbError> {
        self.update_saved_view_at(id, name, filter_json, hidden, now_epoch())
    }

    /// [`Database::update_saved_view`] with an injected clock.
    pub fn update_saved_view_at(
        &self,
        id: &str,
        name: Option<&str>,
        filter_json: Option<&str>,
        hidden: Option<bool>,
        now: i64,
    ) -> Result<SavedView, DbError> {
        self.seed_builtin_saved_views_at(now)?;
        let cur = self
            .get_saved_view(id)?
            .ok_or_else(|| DbError(format!("no saved view {id:?}")))?;
        if cur.built_in && (name.is_some() || filter_json.is_some()) {
            return Err(DbError(
                "a built-in saved view cannot be renamed or re-filtered (only hidden)".into(),
            ));
        }
        let new_name = match name {
            Some(n) => clean_name(n)?,
            None => cur.name.clone(),
        };
        let new_filter = match filter_json {
            Some(j) => parse_view_filter(j)?,
            None => cur.filter.clone(),
        };
        let new_hidden = hidden.unwrap_or(cur.hidden);
        let conn = self.lock_conn();
        let dup: i64 = conn.query_row(
            "SELECT COUNT(*) FROM saved_views WHERE lower(name) = lower(?1) AND id <> ?2",
            params![new_name, id],
            |r| r.get(0),
        )?;
        if dup > 0 {
            return Err(DbError(format!(
                "a saved view named {new_name:?} already exists"
            )));
        }
        conn.execute(
            "UPDATE saved_views SET name = ?2, filter = ?3, hidden = ?4, updated_at = ?5
             WHERE id = ?1",
            params![
                id,
                new_name,
                serde_json::to_string(&new_filter).unwrap_or_default(),
                new_hidden as i64,
                now
            ],
        )?;
        drop(conn);
        self.get_saved_view(id)?
            .ok_or_else(|| DbError("saved view vanished after update".into()))
    }

    /// Delete a custom view. Built-ins cannot be deleted (hide them instead).
    pub fn delete_saved_view(&self, id: &str) -> Result<(), DbError> {
        self.seed_builtin_saved_views()?;
        let cur = self
            .get_saved_view(id)?
            .ok_or_else(|| DbError(format!("no saved view {id:?}")))?;
        if cur.built_in {
            return Err(DbError(
                "a built-in saved view cannot be deleted (hide it instead)".into(),
            ));
        }
        let conn = self.lock_conn();
        conn.execute("DELETE FROM saved_views WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Reorder: the listed ids come first, in the given order; unknown ids
    /// are ignored and views not listed follow in their current order, so a
    /// stale list never loses a view. Positions are rewritten densely 0..n.
    pub fn reorder_saved_views(&self, ordered_ids: &[String]) -> Result<(), DbError> {
        self.seed_builtin_saved_views()?;
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let current: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM saved_views ORDER BY position, id")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<_, _>>()?
        };
        let mut final_order: Vec<&String> = Vec::with_capacity(current.len());
        for id in ordered_ids {
            if let Some(c) = current.iter().find(|c| *c == id) {
                if !final_order.contains(&c) {
                    final_order.push(c);
                }
            }
        }
        for c in &current {
            if !final_order.contains(&c) {
                final_order.push(c);
            }
        }
        for (pos, id) in final_order.iter().enumerate() {
            tx.execute(
                "UPDATE saved_views SET position = ?2 WHERE id = ?1",
                params![id, pos as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The work items of one saved view, via the one evaluator.
    pub fn saved_view_items_tz(
        &self,
        id: &str,
        now: i64,
        tz: i64,
    ) -> Result<Vec<crate::WorkItem>, DbError> {
        self.seed_builtin_saved_views()?;
        let v = self
            .get_saved_view(id)?
            .ok_or_else(|| DbError(format!("no saved view {id:?}")))?;
        self.list_work_items_filtered_tz(&v.filter, now, tz)
    }

    /// Item count of every visible view in ONE query, in display order,
    /// at the current time and local offset.
    pub fn saved_view_counts(&self) -> Result<Vec<SavedViewCount>, DbError> {
        let now = now_epoch();
        self.saved_view_counts_tz(now, local_utc_offset_secs(now))
    }

    /// [`Database::saved_view_counts`] with the clock and UTC offset injected.
    pub fn saved_view_counts_tz(&self, now: i64, tz: i64) -> Result<Vec<SavedViewCount>, DbError> {
        let views = self.list_saved_views(false)?;
        let Some((sql, binds)) = counts_sql(&views, now, tz)? else {
            return Ok(Vec::new());
        };
        let conn = self.lock_conn();
        let counts: Vec<i64> = conn.query_row(&sql, params_from_iter(binds.iter()), |r| {
            (0..views.len()).map(|i| r.get::<_, i64>(i)).collect()
        })?;
        Ok(views
            .into_iter()
            .zip(counts)
            .map(|(v, count)| SavedViewCount { id: v.id, count })
            .collect())
    }

    /// EXPLAIN QUERY PLAN detail lines of the counts statement (test aid).
    pub fn explain_saved_view_counts(&self, now: i64, tz: i64) -> Result<Vec<String>, DbError> {
        let views = self.list_saved_views(false)?;
        let Some((sql, binds)) = counts_sql(&views, now, tz)? else {
            return Ok(Vec::new());
        };
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        let rows = stmt.query_map(params_from_iter(binds.iter()), |r| r.get::<_, String>(3))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkItemUpsert;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);
    const DAY: i64 = 86_400;
    /// Wed 2027-01-13 00:00:00 UTC.
    const WED: i64 = 1_799_798_400;

    fn fresh_db() -> Database {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-saved-views-test-{}-{}.db",
            std::process::id(),
            n
        ));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
        Database::open(&path).expect("open")
    }

    fn names(db: &Database, hidden: bool) -> Vec<String> {
        db.list_saved_views(hidden)
            .unwrap()
            .into_iter()
            .map(|v| v.name)
            .collect()
    }

    fn item(db: &Database, title: &str, state: &str, due: Option<i64>) {
        db.upsert_work_item_at(
            WorkItemUpsert {
                title,
                state,
                due_at: due,
                ..Default::default()
            },
            WED,
        )
        .unwrap();
    }

    #[test]
    fn builtins_exist_once_after_repeated_seeding() {
        let db = fresh_db();
        db.seed_builtin_saved_views().unwrap();
        db.seed_builtin_saved_views().unwrap();
        db.seed_builtin_saved_views_at(WED).unwrap();
        let views = db.list_saved_views(true).unwrap();
        assert_eq!(
            views.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
            ["Today", "Needs reply", "Waiting on others", "Someday"]
        );
        assert!(views.iter().all(|v| v.built_in && !v.hidden));
        assert_eq!(
            views.iter().map(|v| v.position).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
        // Listing seeds on first use too and stays at four.
        assert_eq!(names(&db, true).len(), 4);
    }

    #[test]
    fn seeding_keeps_a_hidden_or_reordered_builtin_as_it_is() {
        let db = fresh_db();
        db.update_saved_view("builtin:someday", None, None, Some(true))
            .unwrap();
        db.reorder_saved_views(&["builtin:someday".to_string()])
            .unwrap();
        db.seed_builtin_saved_views().unwrap();
        let all = db.list_saved_views(true).unwrap();
        assert_eq!(all[0].id, "builtin:someday");
        assert!(all[0].hidden);
        assert_eq!(all.len(), 4);
        assert_eq!(names(&db, false).len(), 3);
    }

    #[test]
    fn builtins_cannot_be_deleted_renamed_or_refiltered_but_can_be_hidden() {
        let db = fresh_db();
        db.seed_builtin_saved_views().unwrap();
        assert!(db.delete_saved_view("builtin:today").is_err());
        assert!(db
            .update_saved_view("builtin:today", Some("Now"), None, None)
            .is_err());
        assert!(db
            .update_saved_view("builtin:today", None, Some("{}"), None)
            .is_err());
        let hidden = db
            .update_saved_view("builtin:today", None, None, Some(true))
            .unwrap();
        assert!(hidden.hidden && hidden.name == "Today");
        assert!(db.get_saved_view("builtin:today").unwrap().is_some());
        // A custom view cannot take a built-in's name (case-insensitive).
        assert!(db.create_saved_view("today", "{}").is_err());
    }

    #[test]
    fn writes_reject_an_invalid_filter_and_store_nothing() {
        let db = fresh_db();
        db.seed_builtin_saved_views().unwrap();
        for bad in [
            r#"{"nonsense":1}"#,
            r#"{"states":["bogus"]}"#,
            r#"{"due":["soon"]}"#,
            r#"{"overdue":"yes"}"#,
            "[]",
            "null",
            "not json",
            "",
        ] {
            assert!(db.create_saved_view("X", bad).is_err(), "accepted {bad:?}");
        }
        assert_eq!(names(&db, true).len(), 4, "nothing was stored");
        let v = db.create_saved_view("Mine", "{}").unwrap();
        let before = db.get_saved_view(&v.id).unwrap().unwrap();
        assert!(db
            .update_saved_view(&v.id, None, Some(r#"{"bad_key":true}"#), None)
            .is_err());
        assert_eq!(db.get_saved_view(&v.id).unwrap().unwrap(), before);
    }

    #[test]
    fn custom_views_create_rename_update_hide_delete() {
        let db = fresh_db();
        let v = db
            .create_saved_view_at("  Chasers  ", r#"{"states":["waiting"]}"#, WED)
            .unwrap();
        assert_eq!(v.name, "Chasers");
        assert!(!v.built_in);
        assert_eq!(v.position, 4, "appended after the four built-ins");
        assert_eq!(
            v.filter.states.as_deref(),
            Some(&["waiting".to_string()][..])
        );
        // Duplicate names (any case) and blank names are refused.
        assert!(db.create_saved_view("chasers", "{}").is_err());
        assert!(db.create_saved_view("   ", "{}").is_err());
        assert!(db.create_saved_view(&"x".repeat(121), "{}").is_err());

        let r = db
            .update_saved_view_at(&v.id, Some("Chase list"), None, None, WED + 5)
            .unwrap();
        assert_eq!((r.name.as_str(), r.updated_at), ("Chase list", WED + 5));
        assert_eq!(r.filter, v.filter, "rename keeps the filter");
        let f = db
            .update_saved_view(&v.id, None, Some(r#"{"waiting_on_me":true}"#), None)
            .unwrap();
        assert_eq!(f.filter.waiting_on_me, Some(true));
        assert!(f.filter.states.is_none(), "filter is replaced, not merged");
        // Renaming onto another view's name is refused; onto itself is fine.
        assert!(db
            .update_saved_view(&v.id, Some("someday"), None, None)
            .is_err());
        db.update_saved_view(&v.id, Some("CHASE LIST"), None, None)
            .unwrap();
        let h = db.update_saved_view(&v.id, None, None, Some(true)).unwrap();
        assert!(h.hidden);
        assert!(!names(&db, false).contains(&"CHASE LIST".to_string()));

        db.delete_saved_view(&v.id).unwrap();
        assert!(db.get_saved_view(&v.id).unwrap().is_none());
        assert!(db.delete_saved_view(&v.id).is_err(), "already gone");
        assert!(db
            .update_saved_view(&v.id, None, None, Some(false))
            .is_err());
    }

    #[test]
    fn reorder_is_dense_tolerant_and_never_loses_a_view() {
        let db = fresh_db();
        let a = db.create_saved_view("A", "{}").unwrap();
        let b = db.create_saved_view("B", "{}").unwrap();
        db.reorder_saved_views(&[
            b.id.clone(),
            "no-such-view".into(),
            a.id.clone(),
            b.id.clone(),
            "builtin:someday".into(),
        ])
        .unwrap();
        assert_eq!(
            names(&db, true),
            [
                "B",
                "A",
                "Someday",
                "Today",
                "Needs reply",
                "Waiting on others"
            ]
        );
        let positions: Vec<i64> = db
            .list_saved_views(true)
            .unwrap()
            .iter()
            .map(|v| v.position)
            .collect();
        assert_eq!(positions, [0, 1, 2, 3, 4, 5]);
        // A stale list (a view deleted meanwhile) still works.
        db.delete_saved_view(&a.id).unwrap();
        db.reorder_saved_views(&[a.id.clone(), "builtin:today".into()])
            .unwrap();
        assert_eq!(
            names(&db, true),
            ["Today", "B", "Someday", "Needs reply", "Waiting on others"]
        );
    }

    #[test]
    fn builtin_counts_follow_the_clock_across_midnight() {
        let db = fresh_db();
        item(&db, "overdue", "needs_reply", Some(WED - 3600));
        item(&db, "tonight", "waiting", Some(WED + 20 * 3600));
        item(&db, "midnight", "delegated", Some(WED + DAY));
        item(&db, "later", "needs_reply", Some(WED + 3 * DAY));
        item(&db, "parked", "someday", Some(WED - DAY));
        item(&db, "finished", "done", Some(WED - DAY));
        item(&db, "undated", "waiting", None);
        let counts = |now: i64| -> Vec<i64> {
            db.saved_view_counts_tz(now, 0)
                .unwrap()
                .iter()
                .map(|c| c.count)
                .collect()
        };
        // [Today, Needs reply, Waiting on others, Someday]
        assert_eq!(counts(WED + 12 * 3600), [2, 2, 3, 1]);
        // Wed 23:59:59: "tonight" is still due today.
        assert_eq!(counts(WED + DAY - 1), [2, 2, 3, 1]);
        // Thu 00:00:00: "tonight" turns overdue and "midnight" is due today:
        // both still in Today.
        assert_eq!(counts(WED + DAY), [3, 2, 3, 1]);
        // Fri: "midnight" is overdue; still in Today. "later" (Sat) is not.
        assert_eq!(counts(WED + 2 * DAY), [3, 2, 3, 1]);
        // Counts agree with listing each view's rows through the evaluator.
        for c in db.saved_view_counts_tz(WED + DAY, 0).unwrap() {
            let rows = db.saved_view_items_tz(&c.id, WED + DAY, 0).unwrap();
            assert_eq!(rows.len() as i64, c.count, "{}", c.id);
        }
    }

    #[test]
    fn counts_skip_hidden_views_and_include_custom_ones_in_order() {
        let db = fresh_db();
        item(&db, "a", "waiting", None);
        item(&db, "b", "blocked", None);
        let v = db
            .create_saved_view("Blocked", r#"{"states":["blocked"]}"#)
            .unwrap();
        db.update_saved_view("builtin:someday", None, None, Some(true))
            .unwrap();
        let counts = db.saved_view_counts_tz(WED, 0).unwrap();
        let ids: Vec<&str> = counts.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "builtin:today",
                "builtin:needs_reply",
                "builtin:waiting_on_others",
                v.id.as_str()
            ]
        );
        assert_eq!(counts.last().unwrap().count, 1);
        // Everything hidden -> an empty list, no query.
        for id in ids {
            db.update_saved_view(id, None, None, Some(true)).unwrap();
        }
        assert!(db.saved_view_counts_tz(WED, 0).unwrap().is_empty());
    }

    #[test]
    fn counts_for_10000_items_are_one_query_and_one_scan() {
        let db = fresh_db();
        let contact = db.create_contact("c@local.test", None).unwrap();
        {
            let conn = db.lock_conn();
            conn.execute_batch("BEGIN").unwrap();
            let states = [
                "needs_reply",
                "waiting",
                "delegated",
                "blocked",
                "done",
                "someday",
            ];
            for i in 0..10_000i64 {
                conn.execute(
                    "INSERT INTO work_items
                         (id, title, notes, state, position, created_at, updated_at, due_at)
                     VALUES (?1, ?2, '', ?3, ?4, ?5, ?5, ?6)",
                    params![
                        format!("w{i:05}"),
                        format!("t{i}"),
                        states[(i % 6) as usize],
                        i % 7,
                        WED,
                        if i % 4 == 0 {
                            None
                        } else {
                            Some(WED + (i % 9 - 3) * DAY)
                        }
                    ],
                )
                .unwrap();
            }
            conn.execute_batch("COMMIT").unwrap();
        }
        // Many views, including one that resolves through the links tables.
        for (i, f) in [
            r#"{"due":["this_week"]}"#,
            r#"{"overdue":true,"waiting_on_others":true}"#,
            r#"{"unassigned":true,"states":["blocked"]}"#,
            r#"{"linked_thread_id":"t1"}"#,
        ]
        .iter()
        .enumerate()
        {
            db.create_saved_view(&format!("V{i}"), f).unwrap();
        }
        db.create_saved_view("Contact", &format!(r#"{{"contact_id":{}}}"#, contact.id))
            .unwrap();

        let plan = db.explain_saved_view_counts(WED, 0).unwrap().join("\n");
        let scans = plan
            .lines()
            .filter(|l| l.contains("SCAN work_items"))
            .count();
        assert_eq!(scans, 1, "exactly one pass over work_items; plan:\n{plan}");
        assert!(!plan.contains("CORRELATED"), "plan:\n{plan}");
        assert!(!plan.contains("SCAN links"), "plan:\n{plan}");

        let start = std::time::Instant::now();
        let counts = db.saved_view_counts_tz(WED, 0).unwrap();
        let took = start.elapsed();
        assert_eq!(counts.len(), 4 + 5);
        // 10_000 rows / 6 states: Needs reply = 1667 (i % 6 == 0).
        assert_eq!(counts[1].count, 1667);
        assert_eq!(counts[2].count, 3334); // waiting (i%6==1) + delegated (i%6==2)
        assert!(
            took < std::time::Duration::from_secs(2),
            "all counts took {took:?}"
        );
    }
}
