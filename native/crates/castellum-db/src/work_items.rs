// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//! Explicit work items (migrations 0018 + 0045) — the Active Work backend.
//!
//! Invariants pinned by tests:
//! - `title`/`notes` are user-owned (only this explicit API writes them —
//!   nothing derives or rewrites them from mail).
//! - `state` is one of the six Active Work states, enforced by CHECK and
//!   validated here for a clean error.
//! - Ordering is deterministic: (state lane order, position, created_at, id);
//!   the filtered list orders by (due_at ascending, NULLs last, position, id).
//! - Deleting a project cascades its work items (FK); deleting a work item
//!   cascades its `links` rows (trigger) — links to messages/threads/contacts
//!   go through the shared link model, never through columns here. The one
//!   column-level contact reference is `delegated_to_contact_id`, which the
//!   FK nulls when the contact is deleted.
//! - `project_id` is nullable (migration 0045): NULL = an unassigned item.
//! - Work items are NEVER created automatically from mail.
//! - This layer stores `waiting_since` / `completed_at` exactly as given; the
//!   state-transition rules that derive them live in castellum-core.

use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::project_timeline::{insert_entry, short, TimelineKind};
use crate::{Database, DbError, EntityKind, LinkEnd, LinkRecord, LinkUpsert};

pub const WORK_ITEM_STATES: [&str; 6] = [
    "needs_reply",
    "waiting",
    "delegated",
    "blocked",
    "done",
    "someday",
];

/// Relation used by the work-item link helpers (`work_item` -> target).
pub const WORK_ITEM_LINK_RELATION: &str = "about";

/// A stored work item. Serialized snake_case for the FFI/Swift boundary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: String,
    /// `None` = unassigned (belongs to no project).
    pub project_id: Option<String>,
    pub title: String,
    pub notes: String,
    pub state: String,
    pub position: i64,
    pub created_at: i64,
    pub updated_at: i64,
    /// Due date, epoch seconds.
    pub due_at: Option<i64>,
    /// Epoch seconds at which the item entered 'waiting'.
    pub waiting_since: Option<i64>,
    /// Epoch seconds at which the item entered 'done'.
    pub completed_at: Option<i64>,
    /// Contact the item was delegated to (state 'delegated').
    pub delegated_to_contact_id: Option<i64>,
}

/// Create/update input. `id: None` creates a new item (uuid-v4). Every
/// optional date/delegate field is written verbatim (None clears it); the
/// core resolves "keep the previous value" and the transition rules before
/// calling this.
#[derive(Clone, Copy, Debug, Default)]
pub struct WorkItemUpsert<'a> {
    pub id: Option<&'a str>,
    pub project_id: Option<&'a str>,
    pub title: &'a str,
    pub notes: &'a str,
    pub state: &'a str,
    pub position: i64,
    pub due_at: Option<i64>,
    pub waiting_since: Option<i64>,
    pub completed_at: Option<i64>,
    pub delegated_to_contact_id: Option<i64>,
}

/// Filter for [`Database::list_work_items_filtered`]. Unknown keys are a
/// parse error (`deny_unknown_fields`), never silently ignored.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkItemFilter {
    /// Items of this project.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Items in any of these states.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub states: Option<Vec<String>>,
    /// `due_at` strictly before this epoch second (items without a due date
    /// never match).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due_before: Option<i64>,
    /// `due_at` strictly after this epoch second.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due_after: Option<i64>,
    /// true = due_at earlier than now and state not done/someday;
    /// false = the complement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overdue: Option<bool>,
    /// Items delegated to, or linked to, this contact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contact_id: Option<i64>,
    /// Items linked to this message id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linked_message_id: Option<String>,
    /// Items linked to this thread id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linked_thread_id: Option<String>,
    /// true = items with no project; false = items with a project.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unassigned: Option<bool>,
    /// Relative due windows, OR-ed together (so a saved view stays correct
    /// tomorrow): `today`, `this_week` (Monday to Sunday) and `overdue`
    /// (the `overdue` rule above). Evaluated against the injected clock in
    /// the local day. An empty list matches nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due: Option<Vec<DueWindow>>,
    /// Shorthand: true = state `needs_reply` (the ball is in my court);
    /// false = any other state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_on_me: Option<bool>,
    /// Shorthand: true = state `waiting` or `delegated`; false = any other.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_on_others: Option<bool>,
}

/// A relative due window of [`WorkItemFilter::due`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DueWindow {
    Today,
    ThisWeek,
    Overdue,
}

const SECS_PER_DAY: i64 = 86_400;

/// Local UTC offset in seconds at `epoch` (0 if the OS cannot say).
pub fn local_utc_offset_secs(epoch: i64) -> i64 {
    let t = epoch as libc::time_t;
    // SAFETY: localtime_r only writes into the zeroed `tm` we own.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if ok {
        tm.tm_gmtoff as i64
    } else {
        0
    }
}

/// `[start, end)` epoch bounds of the local day and of the local Monday-based
/// week containing `now`, for a fixed UTC offset (a DST change inside the
/// window shifts the far edge by an hour at most; the offset is sampled at
/// `now`).
fn day_and_week_bounds(now: i64, tz: i64) -> ((i64, i64), (i64, i64)) {
    let local_day = (now + tz).div_euclid(SECS_PER_DAY);
    // 1970-01-01 was a Thursday; Monday = 0.
    let weekday = (local_day + 3).rem_euclid(7);
    let day_start = local_day * SECS_PER_DAY - tz;
    let week_start = day_start - weekday * SECS_PER_DAY;
    (
        (day_start, day_start + SECS_PER_DAY),
        (week_start, week_start + 7 * SECS_PER_DAY),
    )
}

pub(crate) fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) fn validate_state(state: &str) -> Result<(), DbError> {
    if WORK_ITEM_STATES.contains(&state) {
        Ok(())
    } else {
        Err(DbError(format!(
            "invalid work-item state {state:?} (expected one of {WORK_ITEM_STATES:?})"
        )))
    }
}

pub(crate) fn row_to_item(r: &rusqlite::Row<'_>) -> rusqlite::Result<WorkItem> {
    Ok(WorkItem {
        id: r.get(0)?,
        project_id: r.get(1)?,
        title: r.get(2)?,
        notes: r.get(3)?,
        state: r.get(4)?,
        position: r.get(5)?,
        created_at: r.get(6)?,
        updated_at: r.get(7)?,
        due_at: r.get(8)?,
        waiting_since: r.get(9)?,
        completed_at: r.get(10)?,
        delegated_to_contact_id: r.get(11)?,
    })
}

pub(crate) const ITEM_COLUMNS: &str = "id, project_id, title, notes, state, position, created_at, \
     updated_at, due_at, waiting_since, completed_at, delegated_to_contact_id";

/// Deterministic lane order for listing: active-ish first, parked last.
const STATE_ORDER_SQL: &str = "CASE state
    WHEN 'needs_reply' THEN 0 WHEN 'waiting' THEN 1 WHEN 'delegated' THEN 2
    WHEN 'blocked' THEN 3 WHEN 'done' THEN 4 WHEN 'someday' THEN 5 ELSE 6 END";

/// The WHERE/ORDER part of the filtered list, plus its bind values, built
/// from a validated filter. Kept separate so tests can EXPLAIN it.
fn filtered_sql(f: &WorkItemFilter, now: i64, tz: i64) -> Result<(String, Vec<Value>), DbError> {
    let mut binds: Vec<Value> = Vec::new();
    let clauses = filter_clauses(f, now, tz, &mut binds)?;
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    Ok((
        format!(
            "SELECT {ITEM_COLUMNS} FROM work_items {where_sql}
             ORDER BY (due_at IS NULL), due_at, position, id"
        ),
        binds,
    ))
}

/// The one evaluator: the AND-ed SQL predicates of a validated filter over
/// `work_items`, appending bind values to `binds` (placeholders are numbered
/// from `binds.len()`, so several filters can share one statement, as the
/// saved-view counts do). `now` and the UTC offset `tz` are the injected
/// clock for `overdue` and the relative `due` windows.
pub(crate) fn filter_clauses(
    f: &WorkItemFilter,
    now: i64,
    tz: i64,
    binds: &mut Vec<Value>,
) -> Result<Vec<String>, DbError> {
    let mut clauses: Vec<String> = Vec::new();
    if let Some(p) = &f.project_id {
        binds.push(Value::Text(p.clone()));
        clauses.push(format!("project_id = ?{}", binds.len()));
    }
    if let Some(states) = &f.states {
        for s in states {
            validate_state(s)?;
        }
        if states.is_empty() {
            clauses.push("0".into());
        } else {
            let mut marks = Vec::new();
            for s in states {
                binds.push(Value::Text(s.clone()));
                marks.push(format!("?{}", binds.len()));
            }
            clauses.push(format!("state IN ({})", marks.join(",")));
        }
    }
    if let Some(t) = f.due_before {
        binds.push(Value::Integer(t));
        clauses.push(format!("due_at < ?{}", binds.len()));
    }
    if let Some(t) = f.due_after {
        binds.push(Value::Integer(t));
        clauses.push(format!("due_at > ?{}", binds.len()));
    }
    if let Some(o) = f.overdue {
        let overdue = overdue_clause(now, binds);
        clauses.push(if o { overdue } else { format!("NOT {overdue}") });
    }
    if let Some(windows) = &f.due {
        if windows.is_empty() {
            clauses.push("0".into());
        } else {
            let (day, week) = day_and_week_bounds(now, tz);
            let mut parts = Vec::new();
            for w in windows {
                parts.push(match w {
                    DueWindow::Overdue => overdue_clause(now, binds),
                    DueWindow::Today => range_clause(day, binds),
                    DueWindow::ThisWeek => range_clause(week, binds),
                });
            }
            clauses.push(format!("({})", parts.join(" OR ")));
        }
    }
    if let Some(m) = f.waiting_on_me {
        clauses.push(
            if m {
                "state = 'needs_reply'"
            } else {
                "state <> 'needs_reply'"
            }
            .into(),
        );
    }
    if let Some(o) = f.waiting_on_others {
        clauses.push(
            if o {
                "state IN ('waiting','delegated')"
            } else {
                "state NOT IN ('waiting','delegated')"
            }
            .into(),
        );
    }
    if let Some(u) = f.unassigned {
        clauses.push(
            if u {
                "project_id IS NULL"
            } else {
                "project_id IS NOT NULL"
            }
            .into(),
        );
    }
    if let Some(c) = f.contact_id {
        binds.push(Value::Integer(c));
        let n = binds.len();
        binds.push(Value::Text(c.to_string()));
        let m = binds.len();
        clauses.push(format!(
            "(delegated_to_contact_id = ?{n} OR id IN ({}))",
            linked_ids_sql("contact", m)
        ));
    }
    for (kind, id) in [
        ("message", &f.linked_message_id),
        ("thread", &f.linked_thread_id),
    ] {
        if let Some(id) = id {
            binds.push(Value::Text(id.clone()));
            clauses.push(format!("id IN ({})", linked_ids_sql(kind, binds.len())));
        }
    }
    Ok(clauses)
}

/// `due_at` due earlier than `now` on an item that is still live.
fn overdue_clause(now: i64, binds: &mut Vec<Value>) -> String {
    binds.push(Value::Integer(now));
    let n = binds.len();
    format!("(due_at IS NOT NULL AND due_at < ?{n} AND state NOT IN ('done','someday'))")
}

/// `due_at` inside the half-open `[start, end)` window.
fn range_clause((start, end): (i64, i64), binds: &mut Vec<Value>) -> String {
    binds.push(Value::Integer(start));
    let a = binds.len();
    binds.push(Value::Integer(end));
    let b = binds.len();
    format!("(due_at >= ?{a} AND due_at < ?{b})")
}

/// Subquery: ids of work items linked (either direction) to the `kind`
/// endpoint whose id is bind parameter `?n`. Both arms are index lookups
/// (`idx_links_dst` / `idx_links_src`, forced with INDEXED BY because the
/// unique auto-index otherwise wins and scans every work-item link),
/// evaluated once, not per row.
fn linked_ids_sql(kind: &str, n: usize) -> String {
    format!(
        "SELECT src_id FROM links INDEXED BY idx_links_dst WHERE src_type = 'work_item' AND dst_type = '{kind}' AND dst_id = ?{n}
         UNION
         SELECT dst_id FROM links INDEXED BY idx_links_src WHERE dst_type = 'work_item' AND src_type = '{kind}' AND src_id = ?{n}"
    )
}

impl Database {
    /// Create (`id: None`) or update a work item at the current time.
    /// See [`Database::upsert_work_item_at`].
    pub fn upsert_work_item(&self, w: WorkItemUpsert<'_>) -> Result<WorkItem, DbError> {
        self.upsert_work_item_at(w, now_epoch())
    }

    /// Create or update a work item, stamping `created_at`/`updated_at` with
    /// the injected `now`. Titles must be non-empty; a delegate contact must
    /// exist. Updates rewrite every column given and bump `updated_at`;
    /// `created_at` and project ownership never change on update.
    pub fn upsert_work_item_at(
        &self,
        w: WorkItemUpsert<'_>,
        now: i64,
    ) -> Result<WorkItem, DbError> {
        validate_state(w.state)?;
        if w.title.trim().is_empty() {
            return Err(DbError("work-item title must not be empty".into()));
        }
        let conn = self.lock_conn();
        if let Some(c) = w.delegated_to_contact_id {
            let exists: Option<i64> = conn
                .query_row("SELECT 1 FROM contacts WHERE id = ?1", params![c], |r| {
                    r.get(0)
                })
                .optional()?;
            if exists.is_none() {
                return Err(DbError(format!("contact {c} not found")));
            }
        }
        // Timeline hook: remember the stored state so a transition is logged.
        let mut prev_state: Option<String> = None;
        let mut created = false;
        let id = match w.id.filter(|s| !s.is_empty()) {
            Some(existing) => {
                prev_state = conn
                    .query_row(
                        "SELECT state FROM work_items WHERE id = ?1 AND project_id IS ?2",
                        params![existing, w.project_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                let n = conn.execute(
                    "UPDATE work_items
                     SET title = ?2, notes = ?3, state = ?4, position = ?5,
                         updated_at = ?6, due_at = ?8, waiting_since = ?9,
                         completed_at = ?10, delegated_to_contact_id = ?11
                     WHERE id = ?1 AND project_id IS ?7",
                    params![
                        existing,
                        w.title,
                        w.notes,
                        w.state,
                        w.position,
                        now,
                        w.project_id,
                        w.due_at,
                        w.waiting_since,
                        w.completed_at,
                        w.delegated_to_contact_id
                    ],
                )?;
                if n == 0 {
                    return Err(DbError(format!(
                        "work item {existing} not found in project {}",
                        w.project_id.unwrap_or("(none)")
                    )));
                }
                existing.to_string()
            }
            None => {
                let new_id = Uuid::new_v4().to_string();
                conn.execute(
                    "INSERT INTO work_items
                         (id, project_id, title, notes, state, position,
                          created_at, updated_at, due_at, waiting_since,
                          completed_at, delegated_to_contact_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9, ?10, ?11)",
                    params![
                        new_id,
                        w.project_id,
                        w.title,
                        w.notes,
                        w.state,
                        w.position,
                        now,
                        w.due_at,
                        w.waiting_since,
                        w.completed_at,
                        w.delegated_to_contact_id
                    ],
                )?;
                created = true;
                new_id
            }
        };
        let item = conn.query_row(
            &format!("SELECT {ITEM_COLUMNS} FROM work_items WHERE id = ?1"),
            params![id],
            row_to_item,
        )?;
        // Timeline hook (best effort: the item is already stored, so a
        // timeline failure must not fail the write). Unassigned items belong
        // to no project and log nothing.
        if let Some(pid) = item.project_id.as_deref() {
            let summary = if created {
                Some(format!(
                    "Work item created: {} ({})",
                    short(&item.title),
                    item.state
                ))
            } else {
                match prev_state.as_deref() {
                    Some(old) if old != item.state => Some(format!(
                        "Work item {}: {} -> {}",
                        short(&item.title),
                        old,
                        item.state
                    )),
                    _ => None,
                }
            };
            if let Some(summary) = summary {
                let _ = insert_entry(
                    &conn,
                    pid,
                    TimelineKind::WorkItemChange,
                    &summary,
                    Some((EntityKind::WorkItem, &item.id)),
                    now,
                );
            }
        }
        Ok(item)
    }

    /// A project's work items in deterministic order:
    /// state lane (needs_reply … someday), then position, created_at, id.
    pub fn list_work_items(&self, project_id: &str) -> Result<Vec<WorkItem>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {ITEM_COLUMNS} FROM work_items
             WHERE project_id = ?1
             ORDER BY {STATE_ORDER_SQL}, position, created_at, id"
        ))?;
        let rows = stmt.query_map(params![project_id], row_to_item)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Work items matching `filter` at the current time.
    /// See [`Database::list_work_items_filtered_at`].
    pub fn list_work_items_filtered(
        &self,
        filter: &WorkItemFilter,
    ) -> Result<Vec<WorkItem>, DbError> {
        self.list_work_items_filtered_at(filter, now_epoch())
    }

    /// Work items matching every given filter key (AND), ordered by `due_at`
    /// ascending with NULLs last, then `position`, then `id`. `now` is the
    /// injected clock used by `overdue`.
    pub fn list_work_items_filtered_at(
        &self,
        filter: &WorkItemFilter,
        now: i64,
    ) -> Result<Vec<WorkItem>, DbError> {
        self.list_work_items_filtered_tz(filter, now, local_utc_offset_secs(now))
    }

    /// [`Database::list_work_items_filtered_at`] with the UTC offset (seconds)
    /// injected as well, so the `today`/`this_week` windows are deterministic.
    pub fn list_work_items_filtered_tz(
        &self,
        filter: &WorkItemFilter,
        now: i64,
        tz: i64,
    ) -> Result<Vec<WorkItem>, DbError> {
        let (sql, binds) = filtered_sql(filter, now, tz)?;
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(binds.iter()), row_to_item)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// EXPLAIN QUERY PLAN detail lines for a filter (test/diagnostic aid).
    pub fn explain_work_items_filtered(
        &self,
        filter: &WorkItemFilter,
        now: i64,
    ) -> Result<Vec<String>, DbError> {
        let (sql, binds) = filtered_sql(filter, now, local_utc_offset_secs(now))?;
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        let rows = stmt.query_map(params_from_iter(binds.iter()), |r| r.get::<_, String>(3))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// One work item by id.
    pub fn get_work_item(&self, id: &str) -> Result<Option<WorkItem>, DbError> {
        let conn = self.lock_conn();
        let row = conn
            .query_row(
                &format!("SELECT {ITEM_COLUMNS} FROM work_items WHERE id = ?1"),
                params![id],
                row_to_item,
            )
            .optional()?;
        Ok(row)
    }

    /// Permanently delete a work item (its `links` rows cascade via trigger).
    pub fn delete_work_item(&self, id: &str) -> Result<(), DbError> {
        let conn = self.lock_conn();
        let prior: Option<(Option<String>, String)> = conn
            .query_row(
                "SELECT project_id, title FROM work_items WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let n = conn.execute("DELETE FROM work_items WHERE id = ?1", params![id])?;
        if n == 0 {
            return Err(DbError(format!("work item {id} not found")));
        }
        // Timeline hook (best effort, see upsert_work_item_at).
        if let Some((Some(pid), title)) = prior {
            let _ = insert_entry(
                &conn,
                &pid,
                TimelineKind::WorkItemChange,
                &format!("Work item deleted: {}", short(&title)),
                Some((EntityKind::WorkItem, id)),
                now_epoch(),
            );
        }
        Ok(())
    }

    /// Only message, thread and contact may be attached to a work item.
    fn check_work_item_target(kind: EntityKind) -> Result<(), DbError> {
        match kind {
            EntityKind::Message | EntityKind::Thread | EntityKind::Contact => Ok(()),
            other => Err(DbError(format!(
                "a work item can only link to a message, thread or contact, not {:?}",
                other.as_str()
            ))),
        }
    }

    /// Link a work item to a message, thread or contact (manual origin,
    /// relation `about`, work item as the source). Idempotent: repeating the
    /// call returns the same row. Endpoint existence follows the link-model
    /// integrity rules (a missing work item, message or contact is an error;
    /// threads are virtual and always accepted).
    pub fn link_work_item(
        &self,
        work_item_id: &str,
        kind: EntityKind,
        target_id: &str,
    ) -> Result<LinkRecord, DbError> {
        Self::check_work_item_target(kind)?;
        // Link and (for messages) its "came from" provenance row commit
        // together: a provenance failure rolls the link back. The timeline
        // entry (a message newly attached to a project's work item) joins
        // the same transaction.
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        // The "already linked" check lives inside the transaction (and the
        // connection mutex), so two racing callers cannot both see "new" and
        // each write a timeline row.
        let existed = crate::links::link_between_in(
            &tx,
            LinkEnd::new(EntityKind::WorkItem, work_item_id),
            LinkEnd::new(kind, target_id),
            WORK_ITEM_LINK_RELATION,
        )?
        .is_some();
        let rec = crate::links::upsert_link_in(
            &tx,
            LinkUpsert {
                src: LinkEnd::new(EntityKind::WorkItem, work_item_id),
                dst: LinkEnd::new(kind, target_id),
                relation: WORK_ITEM_LINK_RELATION,
                origin: "manual",
                provenance: None,
                confidence: None,
            },
        )?;
        // Idempotent; work_items.notes is never touched.
        if kind == EntityKind::Message {
            crate::provenance::record_in(&tx, work_item_id, "message", target_id, now_epoch())?;
        }
        if !existed && kind == EntityKind::Message {
            let owner: Option<(Option<String>, String)> = tx
                .query_row(
                    "SELECT project_id, title FROM work_items WHERE id = ?1",
                    params![work_item_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((Some(pid), title)) = owner {
                let _ = insert_entry(
                    &tx,
                    &pid,
                    TimelineKind::WorkItemChange,
                    &format!("Message linked to work item: {}", short(&title)),
                    Some((EntityKind::Message, target_id)),
                    now_epoch(),
                );
            }
        }
        tx.commit()?;
        Ok(rec)
    }

    /// Remove a work item's link to a message, thread or contact. Returns
    /// whether a row was removed (false = nothing to do, still Ok).
    pub fn unlink_work_item(
        &self,
        work_item_id: &str,
        kind: EntityKind,
        target_id: &str,
    ) -> Result<bool, DbError> {
        Self::check_work_item_target(kind)?;
        self.unlink(
            LinkEnd::new(EntityKind::WorkItem, work_item_id),
            LinkEnd::new(kind, target_id),
            WORK_ITEM_LINK_RELATION,
        )
    }

    /// Reverse lookup: the work items of a message, a thread or a contact
    /// (a contact also matches items delegated to it), in the filtered-list
    /// order. An unknown id yields an empty list; a wrong kind is an error.
    pub fn work_items_for(&self, kind: EntityKind, id: &str) -> Result<Vec<WorkItem>, DbError> {
        Self::check_work_item_target(kind)?;
        let mut f = WorkItemFilter::default();
        match kind {
            EntityKind::Message => f.linked_message_id = Some(id.to_string()),
            EntityKind::Thread => f.linked_thread_id = Some(id.to_string()),
            _ => {
                let c: i64 = id
                    .parse()
                    .map_err(|_| DbError(format!("invalid contact id {id:?}")))?;
                f.contact_id = Some(c);
            }
        }
        self.list_work_items_filtered(&f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EntityKind, LinkEnd, LinkUpsert};
    use castellum_models::{Account, Mailbox, NewMessage};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Seed one real message so link-model endpoint validation is satisfied.
    fn seed_message(db: &Database, id: &str) {
        db.upsert_account(&Account {
            id: "acc".into(),
            email: "me@local.test".into(),
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
        db.upsert_message(&NewMessage {
            id: id.into(),
            account_id: "acc".into(),
            mailbox_id: "acc:INBOX".into(),
            provider_uid: Some(1),
            rfc822_message_id: Some("<1@local.test>".into()),
            subject: Some("S".into()),
            from_name: None,
            from_address: Some("s@local.test".into()),
            sent_at: Some(1_800_000_000),
            snippet: None,
            size_bytes: None,
            has_attachments: false,
            attachments: None,
            is_read: false,
            is_starred: false,
            addresses: Vec::new(),
            text_body: None,
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: Vec::new(),
        })
        .unwrap();
    }

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn fresh_db() -> Database {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-work-items-test-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        Database::open(&path).expect("open")
    }

    fn project(db: &Database) -> String {
        db.upsert_project(crate::ProjectUpsert {
            name: "P",
            status: "active",
            position: 0,
            ..Default::default()
        })
        .unwrap()
        .id
    }

    fn item<'a>(project_id: &'a str, title: &'a str, state: &'a str) -> WorkItemUpsert<'a> {
        WorkItemUpsert {
            id: None,
            project_id: Some(project_id),
            title,
            notes: "",
            state,
            position: 0,
            ..Default::default()
        }
    }

    #[test]
    fn create_update_get_delete_round_trip() {
        let db = fresh_db();
        let pid = project(&db);
        let created = db
            .upsert_work_item(item(&pid, "Chase invoice", "waiting"))
            .unwrap();
        assert_eq!(created.state, "waiting");
        assert!(!created.id.is_empty());
        assert_eq!(created.created_at, created.updated_at);

        let updated = db
            .upsert_work_item(WorkItemUpsert {
                id: Some(&created.id),
                project_id: Some(&pid),
                title: "Chase invoice #42",
                notes: "asked twice",
                state: "blocked",
                position: 3,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(updated.id, created.id);
        assert_eq!(updated.title, "Chase invoice #42");
        assert_eq!(updated.state, "blocked");
        assert_eq!(
            updated.created_at, created.created_at,
            "created_at never changes"
        );

        assert!(db.get_work_item(&created.id).unwrap().is_some());
        db.delete_work_item(&created.id).unwrap();
        assert!(db.get_work_item(&created.id).unwrap().is_none());
        assert!(
            db.delete_work_item(&created.id).is_err(),
            "double delete errors"
        );
    }

    #[test]
    fn invalid_state_and_empty_title_are_rejected() {
        let db = fresh_db();
        let pid = project(&db);
        assert!(db.upsert_work_item(item(&pid, "T", "doing")).is_err());
        assert!(db.upsert_work_item(item(&pid, "  ", "waiting")).is_err());
        // Unknown project FK is rejected.
        assert!(db.upsert_work_item(item("nope", "T", "waiting")).is_err());
        // Updating an item under the WRONG project id is refused.
        let created = db.upsert_work_item(item(&pid, "T", "waiting")).unwrap();
        assert!(db
            .upsert_work_item(WorkItemUpsert {
                id: Some(&created.id),
                project_id: Some("other-project"),
                title: "T",
                notes: "",
                state: "waiting",
                position: 0,
                ..Default::default()
            })
            .is_err());
    }

    #[test]
    fn listing_order_is_deterministic_across_states_and_ties() {
        let db = fresh_db();
        let pid = project(&db);
        let done = db.upsert_work_item(item(&pid, "d", "done")).unwrap();
        let blocked = db.upsert_work_item(item(&pid, "b", "blocked")).unwrap();
        let reply = db.upsert_work_item(item(&pid, "r", "needs_reply")).unwrap();
        let reply2 = db
            .upsert_work_item(WorkItemUpsert {
                position: 1,
                ..item(&pid, "r2", "needs_reply")
            })
            .unwrap();
        let listed = db.list_work_items(&pid).unwrap();
        assert_eq!(
            listed.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(),
            vec![
                reply.id.as_str(),   // needs_reply pos 0
                reply2.id.as_str(),  // needs_reply pos 1
                blocked.id.as_str(), // blocked
                done.id.as_str(),    // done
            ],
            "lane order then position then created_at/id"
        );
    }

    /// Rows in a project's timeline that announce a message link.
    fn linked_timeline_rows(db: &Database, pid: &str) -> usize {
        db.list_timeline(pid, None, 100, 0)
            .unwrap()
            .iter()
            .filter(|e| e.summary.starts_with("Message linked to work item"))
            .count()
    }

    #[test]
    fn relinking_a_message_writes_one_timeline_and_one_provenance_row() {
        let db = fresh_db();
        let pid = project(&db);
        seed_message(&db, "acc:INBOX:1");
        let w = db.upsert_work_item(item(&pid, "T", "waiting")).unwrap();
        for _ in 0..2 {
            db.link_work_item(&w.id, EntityKind::Message, "acc:INBOX:1")
                .unwrap();
        }
        assert_eq!(linked_timeline_rows(&db, &pid), 1);
        assert_eq!(db.list_work_item_provenance(&w.id).unwrap().len(), 1);
    }

    /// Smoke test: eight threads link the same pair and exactly one timeline
    /// row and one provenance row result. This does NOT prove the link is
    /// race-free: the connection mutex already serialises the callers and the
    /// pre-fix race window was tiny, so it would mostly pass without the fix.
    /// Atomicity is covered by `provenance_failure_rolls_back_the_link`.
    #[test]
    fn concurrent_links_of_the_same_pair_write_one_timeline_row() {
        let db = std::sync::Arc::new(fresh_db());
        let pid = project(&db);
        seed_message(&db, "acc:INBOX:1");
        let w = db.upsert_work_item(item(&pid, "T", "waiting")).unwrap();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let db = std::sync::Arc::clone(&db);
                let wid = w.id.clone();
                std::thread::spawn(move || {
                    db.link_work_item(&wid, EntityKind::Message, "acc:INBOX:1")
                        .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(linked_timeline_rows(&db, &pid), 1);
        assert_eq!(db.list_work_item_provenance(&w.id).unwrap().len(), 1);
    }

    #[test]
    fn provenance_failure_rolls_back_the_link() {
        let db = fresh_db();
        let pid = project(&db);
        seed_message(&db, "acc:INBOX:9");
        let w = db.upsert_work_item(item(&pid, "T", "waiting")).unwrap();
        // Force the provenance insert to fail.
        db.lock_conn()
            .execute_batch(
                "CREATE TRIGGER force_prov_fail BEFORE INSERT ON work_item_provenance
                 BEGIN SELECT RAISE(ABORT, 'forced'); END;",
            )
            .unwrap();
        assert!(db
            .link_work_item(&w.id, EntityKind::Message, "acc:INBOX:9")
            .is_err());
        let links = db
            .links_for(LinkEnd::new(EntityKind::WorkItem, &w.id))
            .unwrap();
        assert!(links.is_empty(), "link must roll back with provenance");
    }

    #[test]
    fn project_delete_cascades_items_and_item_delete_cascades_links() {
        let db = fresh_db();
        let pid = project(&db);
        seed_message(&db, "acc:INBOX:1");
        let w = db.upsert_work_item(item(&pid, "T", "waiting")).unwrap();
        // Link the item to a message through the shared link model.
        db.upsert_link(LinkUpsert {
            src: LinkEnd::new(EntityKind::WorkItem, &w.id),
            dst: LinkEnd::new(EntityKind::Message, "acc:INBOX:1"),
            relation: "tracks",
            origin: "manual",
            provenance: None,
            confidence: None,
        })
        .unwrap();

        // Deleting the work item removes its link rows (0018 trigger).
        db.delete_work_item(&w.id).unwrap();
        assert!(db
            .links_for(LinkEnd::new(EntityKind::WorkItem, &w.id))
            .unwrap()
            .is_empty());

        // Deleting the project cascades remaining items (FK).
        let w2 = db.upsert_work_item(item(&pid, "T2", "waiting")).unwrap();
        db.delete_project(&pid).unwrap();
        assert!(db.get_work_item(&w2.id).unwrap().is_none());
    }

    // ---- AW1: dates, filters, link helpers -------------------------------

    const NOW: i64 = 1_800_000_000;

    fn mk(
        db: &Database,
        pid: Option<&str>,
        title: &str,
        state: &str,
        due: Option<i64>,
    ) -> WorkItem {
        db.upsert_work_item_at(
            WorkItemUpsert {
                project_id: pid,
                title,
                state,
                due_at: due,
                ..Default::default()
            },
            NOW,
        )
        .unwrap()
    }

    fn titles(items: &[WorkItem]) -> Vec<String> {
        items.iter().map(|w| w.title.clone()).collect()
    }

    fn filter(json: &str) -> WorkItemFilter {
        serde_json::from_str(json).unwrap()
    }

    fn list(db: &Database, json: &str) -> Vec<String> {
        titles(&db.list_work_items_filtered_at(&filter(json), NOW).unwrap())
    }

    #[test]
    fn dates_and_delegate_round_trip_and_null_by_default() {
        let db = fresh_db();
        let pid = project(&db);
        let c = db.create_contact("d@local.test", None).unwrap();
        let plain = db
            .upsert_work_item(item(&pid, "plain", "needs_reply"))
            .unwrap();
        assert_eq!(
            (
                plain.due_at,
                plain.waiting_since,
                plain.completed_at,
                plain.delegated_to_contact_id
            ),
            (None, None, None, None)
        );
        let w = db
            .upsert_work_item_at(
                WorkItemUpsert {
                    project_id: Some(&pid),
                    title: "T",
                    state: "delegated",
                    due_at: Some(NOW + 5),
                    waiting_since: Some(NOW - 1),
                    completed_at: Some(NOW - 2),
                    delegated_to_contact_id: Some(c.id),
                    ..Default::default()
                },
                NOW,
            )
            .unwrap();
        assert_eq!(w.due_at, Some(NOW + 5));
        assert_eq!(w.waiting_since, Some(NOW - 1));
        assert_eq!(w.completed_at, Some(NOW - 2));
        assert_eq!(w.delegated_to_contact_id, Some(c.id));
        assert_eq!(w.created_at, NOW, "injected clock stamps created_at");
    }

    #[test]
    fn delegate_must_exist_and_is_nulled_when_the_contact_is_deleted() {
        let db = fresh_db();
        let pid = project(&db);
        let bad = db.upsert_work_item(WorkItemUpsert {
            project_id: Some(&pid),
            title: "T",
            state: "delegated",
            delegated_to_contact_id: Some(9999),
            ..Default::default()
        });
        assert!(bad.is_err(), "unknown contact rejected");
        let c = db.create_contact("d@local.test", None).unwrap();
        let w = db
            .upsert_work_item(WorkItemUpsert {
                project_id: Some(&pid),
                title: "T",
                state: "delegated",
                delegated_to_contact_id: Some(c.id),
                ..Default::default()
            })
            .unwrap();
        db.lock_conn()
            .execute("DELETE FROM contacts WHERE id = ?1", params![c.id])
            .unwrap();
        let after = db.get_work_item(&w.id).unwrap().expect("item survives");
        assert_eq!(after.delegated_to_contact_id, None);
        assert_eq!(after.title, "T");
    }

    #[test]
    fn filter_project_id() {
        let db = fresh_db();
        let a = project(&db);
        let b = db
            .upsert_project(crate::ProjectUpsert {
                name: "B",
                status: "active",
                position: 1,
                ..Default::default()
            })
            .unwrap()
            .id;
        mk(&db, Some(&a), "in-a", "waiting", None);
        mk(&db, Some(&b), "in-b", "waiting", None);
        assert_eq!(
            list(&db, &format!(r#"{{"project_id":"{a}"}}"#)),
            vec!["in-a"]
        );
        assert_eq!(
            list(&db, &format!(r#"{{"project_id":"{b}"}}"#)),
            vec!["in-b"]
        );
    }

    #[test]
    fn filter_states() {
        let db = fresh_db();
        let p = project(&db);
        mk(&db, Some(&p), "w", "waiting", None);
        mk(&db, Some(&p), "b", "blocked", None);
        mk(&db, Some(&p), "d", "done", None);
        let mut got = list(&db, r#"{"states":["waiting","done"]}"#);
        got.sort();
        assert_eq!(got, vec!["d", "w"]);
        assert!(
            list(&db, r#"{"states":[]}"#).is_empty(),
            "empty set matches nothing"
        );
        let bad = db.list_work_items_filtered(&filter(r#"{"states":["doing"]}"#));
        assert!(bad.is_err(), "unknown state rejected");
    }

    #[test]
    fn filter_due_before_and_due_after() {
        let db = fresh_db();
        let p = project(&db);
        mk(&db, Some(&p), "early", "waiting", Some(100));
        mk(&db, Some(&p), "mid", "waiting", Some(200));
        mk(&db, Some(&p), "late", "waiting", Some(300));
        mk(&db, Some(&p), "undated", "waiting", None);
        assert_eq!(list(&db, r#"{"due_before":200}"#), vec!["early"]);
        assert_eq!(list(&db, r#"{"due_after":200}"#), vec!["late"]);
        assert_eq!(
            list(&db, r#"{"due_after":100,"due_before":300}"#),
            vec!["mid"],
            "keys combine with AND"
        );
    }

    #[test]
    fn filter_overdue_uses_the_injected_clock_and_skips_done_and_someday() {
        let db = fresh_db();
        let p = project(&db);
        mk(&db, Some(&p), "late-open", "needs_reply", Some(NOW - 10));
        mk(&db, Some(&p), "late-done", "done", Some(NOW - 10));
        mk(&db, Some(&p), "late-someday", "someday", Some(NOW - 10));
        mk(&db, Some(&p), "future", "needs_reply", Some(NOW + 10));
        mk(&db, Some(&p), "undated", "needs_reply", None);
        assert_eq!(list(&db, r#"{"overdue":true}"#), vec!["late-open"]);
        let mut rest = list(&db, r#"{"overdue":false}"#);
        rest.sort();
        assert_eq!(rest, vec!["future", "late-done", "late-someday", "undated"]);
        // Moving the clock forward makes the future item overdue.
        let later = db
            .list_work_items_filtered_at(&filter(r#"{"overdue":true}"#), NOW + 20)
            .unwrap();
        assert_eq!(titles(&later), vec!["late-open", "future"]);
    }

    #[test]
    fn filter_contact_id_matches_delegate_and_links() {
        let db = fresh_db();
        let p = project(&db);
        let c = db.create_contact("c@local.test", None).unwrap();
        let other = db.create_contact("o@local.test", None).unwrap();
        db.upsert_work_item(WorkItemUpsert {
            project_id: Some(&p),
            title: "delegated",
            state: "delegated",
            delegated_to_contact_id: Some(c.id),
            ..Default::default()
        })
        .unwrap();
        let linked = mk(&db, Some(&p), "linked", "waiting", None);
        mk(&db, Some(&p), "unrelated", "waiting", None);
        let other_item = mk(&db, Some(&p), "other", "waiting", None);
        db.link_work_item(&linked.id, EntityKind::Contact, &c.id.to_string())
            .unwrap();
        db.link_work_item(&other_item.id, EntityKind::Contact, &other.id.to_string())
            .unwrap();
        let mut got = list(&db, &format!(r#"{{"contact_id":{}}}"#, c.id));
        got.sort();
        assert_eq!(got, vec!["delegated", "linked"]);
    }

    #[test]
    fn filter_linked_message_id_and_linked_thread_id() {
        let db = fresh_db();
        let p = project(&db);
        seed_message(&db, "acc:INBOX:1");
        let a = mk(&db, Some(&p), "on-message", "waiting", None);
        let b = mk(&db, Some(&p), "on-thread", "waiting", None);
        mk(&db, Some(&p), "bare", "waiting", None);
        db.link_work_item(&a.id, EntityKind::Message, "acc:INBOX:1")
            .unwrap();
        db.link_work_item(&b.id, EntityKind::Thread, "thread-1")
            .unwrap();
        assert_eq!(
            list(&db, r#"{"linked_message_id":"acc:INBOX:1"}"#),
            vec!["on-message"]
        );
        assert_eq!(
            list(&db, r#"{"linked_thread_id":"thread-1"}"#),
            vec!["on-thread"]
        );
        assert!(list(&db, r#"{"linked_thread_id":"nope"}"#).is_empty());
    }

    #[test]
    fn filter_unassigned_selects_items_with_no_project() {
        let db = fresh_db();
        let p = project(&db);
        mk(&db, Some(&p), "assigned", "waiting", None);
        let loose = mk(&db, None, "loose", "waiting", None);
        assert_eq!(loose.project_id, None);
        assert_eq!(list(&db, r#"{"unassigned":true}"#), vec!["loose"]);
        assert_eq!(list(&db, r#"{"unassigned":false}"#), vec!["assigned"]);
        // An unassigned item can be updated (project_id IS NULL matches).
        let upd = db
            .upsert_work_item_at(
                WorkItemUpsert {
                    id: Some(&loose.id),
                    project_id: None,
                    title: "loose 2",
                    state: "waiting",
                    ..Default::default()
                },
                NOW + 1,
            )
            .unwrap();
        assert_eq!(upd.title, "loose 2");
    }

    #[test]
    fn unknown_filter_keys_are_rejected_not_ignored() {
        let err = serde_json::from_str::<WorkItemFilter>(r#"{"project":"x"}"#).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
        let err =
            serde_json::from_str::<WorkItemFilter>(r#"{"states":["done"],"bogus":1}"#).unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn filtered_order_is_due_ascending_nulls_last_then_position_then_id() {
        let db = fresh_db();
        let p = project(&db);
        let ins = |title: &str, due: Option<i64>, pos: i64| {
            db.upsert_work_item_at(
                WorkItemUpsert {
                    project_id: Some(&p),
                    title,
                    state: "waiting",
                    due_at: due,
                    position: pos,
                    ..Default::default()
                },
                NOW,
            )
            .unwrap()
        };
        ins("undated-p1", None, 1);
        ins("late", Some(300), 0);
        ins("early-p1", Some(100), 1);
        ins("early-p0", Some(100), 0);
        ins("undated-p0", None, 0);
        assert_eq!(
            list(&db, "{}"),
            vec!["early-p0", "early-p1", "late", "undated-p0", "undated-p1"]
        );
        // Equal due and position: id breaks the tie, deterministically.
        let a = ins("tie-a", Some(50), 0);
        let b = ins("tie-b", Some(50), 0);
        let got = db.list_work_items_filtered_at(&filter("{}"), NOW).unwrap();
        let (first, second) = if a.id < b.id { (&a, &b) } else { (&b, &a) };
        assert_eq!(got[0].id, first.id);
        assert_eq!(got[1].id, second.id);
    }

    #[test]
    fn filtered_list_with_10000_items_uses_indexes_and_stays_fast() {
        let db = fresh_db();
        let p = project(&db);
        seed_message(&db, "acc:INBOX:1");
        {
            let conn = db.lock_conn();
            conn.execute_batch("BEGIN").unwrap();
            for i in 0..10_000i64 {
                conn.execute(
                    "INSERT INTO work_items
                         (id, project_id, title, notes, state, position,
                          created_at, updated_at, due_at)
                     VALUES (?1, ?2, ?3, '', 'waiting', ?4, ?5, ?5, ?6)",
                    params![
                        format!("w{i:05}"),
                        p,
                        format!("t{i}"),
                        i % 7,
                        NOW,
                        if i % 3 == 0 { None } else { Some(NOW + i) }
                    ],
                )
                .unwrap();
            }
            conn.execute_batch("COMMIT").unwrap();
        }
        db.link_work_item("w00042", EntityKind::Message, "acc:INBOX:1")
            .unwrap();

        // The linked-message filter resolves through the links indexes once
        // (a subquery), not through a per-row scan of `links`.
        let f = filter(r#"{"linked_message_id":"acc:INBOX:1"}"#);
        let plan = db.explain_work_items_filtered(&f, NOW).unwrap().join("\n");
        assert!(
            plan.contains("idx_links_dst") && plan.contains("idx_links_src"),
            "plan: {plan}"
        );
        assert!(!plan.contains("SCAN links"), "plan: {plan}");
        // The unfiltered list walks the due-order index (no temp B-tree sort).
        let plan = db
            .explain_work_items_filtered(&WorkItemFilter::default(), NOW)
            .unwrap()
            .join("\n");
        assert!(plan.contains("idx_work_items_due"), "plan: {plan}");
        assert!(!plan.contains("TEMP B-TREE"), "plan: {plan}");
        // A project filter is an index search, not a table scan.
        let f_project = WorkItemFilter {
            project_id: Some(p.clone()),
            states: Some(vec!["waiting".into()]),
            ..Default::default()
        };
        let plan = db
            .explain_work_items_filtered(&f_project, NOW)
            .unwrap()
            .join("\n");
        assert!(
            plan.contains("SEARCH work_items USING INDEX"),
            "plan: {plan}"
        );

        let t = std::time::Instant::now();
        let linked = db.list_work_items_filtered_at(&f, NOW).unwrap();
        let overdue = db
            .list_work_items_filtered_at(&filter(r#"{"overdue":true}"#), NOW + 5_000)
            .unwrap();
        let all = db.list_work_items_filtered_at(&filter("{}"), NOW).unwrap();
        assert!(
            t.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            t.elapsed()
        );
        assert_eq!(titles(&linked), vec!["t42"]);
        assert_eq!(all.len(), 10_000);
        assert!(!overdue.is_empty());
        // Nulls last across the whole set.
        let first_null = all.iter().position(|w| w.due_at.is_none()).unwrap();
        assert!(all[first_null..].iter().all(|w| w.due_at.is_none()));
        assert!(all[..first_null]
            .windows(2)
            .all(|p| p[0].due_at <= p[1].due_at));
    }

    #[test]
    fn link_helpers_are_idempotent_and_reverse_lookups_work() {
        let db = fresh_db();
        let p = project(&db);
        seed_message(&db, "acc:INBOX:1");
        let c = db.create_contact("c@local.test", None).unwrap();
        let w = mk(&db, Some(&p), "W", "waiting", None);
        let cid = c.id.to_string();

        let first = db
            .link_work_item(&w.id, EntityKind::Message, "acc:INBOX:1")
            .unwrap();
        let again = db
            .link_work_item(&w.id, EntityKind::Message, "acc:INBOX:1")
            .unwrap();
        assert_eq!(first.id, again.id, "idempotent: same row");
        assert_eq!(first.origin, "manual");
        db.link_work_item(&w.id, EntityKind::Thread, "thr-1")
            .unwrap();
        db.link_work_item(&w.id, EntityKind::Contact, &cid).unwrap();
        assert_eq!(
            db.links_for(LinkEnd::new(EntityKind::WorkItem, &w.id))
                .unwrap()
                .len(),
            3
        );

        for (kind, id) in [
            (EntityKind::Message, "acc:INBOX:1"),
            (EntityKind::Thread, "thr-1"),
            (EntityKind::Contact, cid.as_str()),
        ] {
            let got = db.work_items_for(kind, id).unwrap();
            assert_eq!(titles(&got), vec!["W"], "{kind:?}");
        }
        assert!(db
            .work_items_for(EntityKind::Thread, "unknown")
            .unwrap()
            .is_empty());

        assert!(db
            .unlink_work_item(&w.id, EntityKind::Thread, "thr-1")
            .unwrap());
        assert!(
            !db.unlink_work_item(&w.id, EntityKind::Thread, "thr-1")
                .unwrap(),
            "unlinking twice is a no-op"
        );
        assert!(db
            .work_items_for(EntityKind::Thread, "thr-1")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn link_helpers_reject_hostile_input() {
        let db = fresh_db();
        let p = project(&db);
        seed_message(&db, "acc:INBOX:1");
        let w = mk(&db, Some(&p), "W", "waiting", None);
        // Missing endpoints.
        assert!(db
            .link_work_item("no-such-item", EntityKind::Message, "acc:INBOX:1")
            .is_err());
        assert!(db
            .link_work_item(&w.id, EntityKind::Message, "acc:INBOX:404")
            .is_err());
        assert!(db
            .link_work_item(&w.id, EntityKind::Contact, "999")
            .is_err());
        assert!(db
            .link_work_item(&w.id, EntityKind::Contact, "not-a-number")
            .is_err());
        // Wrong endpoint kinds.
        assert!(db.link_work_item(&w.id, EntityKind::Project, &p).is_err());
        assert!(db
            .link_work_item(&w.id, EntityKind::WorkItem, &w.id)
            .is_err());
        assert!(db.unlink_work_item(&w.id, EntityKind::Project, &p).is_err());
        assert!(db.work_items_for(EntityKind::Project, &p).is_err());
        assert!(db.work_items_for(EntityKind::WorkItem, &w.id).is_err());
        assert!(db
            .work_items_for(EntityKind::Contact, "x'; DROP TABLE work_items;--")
            .is_err());
        // Empty / oversized ids never reach a write.
        assert!(db.link_work_item(&w.id, EntityKind::Thread, "").is_err());
        assert!(db
            .link_work_item(&w.id, EntityKind::Thread, &"x".repeat(600))
            .is_err());
        // Nothing was written by any rejected call.
        assert!(db
            .links_for(LinkEnd::new(EntityKind::WorkItem, &w.id))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn migration_0045_rebuild_keeps_rows_and_links() {
        // A v44 profile: put the pre-0045 table shape back with one item and
        // one link, forget 0045, reopen, and check nothing was lost.
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-work-items-0045-{}.db",
            std::process::id()
        ));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
        {
            let db = Database::open(&path).unwrap();
            let p = project(&db);
            db.lock_conn()
                .execute_batch(&format!(
                    "DROP TABLE work_items;
                     {}
                     INSERT INTO work_items (id, project_id, title, notes, state, position, created_at, updated_at)
                       VALUES ('old1', '{p}', 'Old title', 'old notes', 'blocked', 4, 10, 11);
                     INSERT INTO links (src_type, src_id, dst_type, dst_id, relation, origin, created_at, updated_at)
                       VALUES ('work_item', 'old1', 'thread', 't', 'about', 'manual', 1, 1);
                     DROP TABLE suggestions;
                     DROP TABLE saved_views;
                     DROP TABLE project_timeline;
                 DROP INDEX idx_messages_account_rfc822;
                 DROP TABLE work_item_provenance;
                     DROP TABLE notification_outbox;
                     DROP TABLE vip_senders;
                     ALTER TABLE calendar_invites DROP COLUMN location;
                     ALTER TABLE calendar_invites DROP COLUMN description;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_raw;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_tzid;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_is_utc;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_is_date_only;
                     DROP TABLE pending_refetch;
                     DROP TABLE pending_rules;
                     DROP TABLE message_header_capture;
                     DROP TABLE message_headers;
                     DROP TABLE header_names;
                     DELETE FROM _migrations WHERE id IN (45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57);",
                    include_str!("migrations/0018_work_items.sql")
                ))
                .unwrap();
        }
        let db = Database::open(&path).unwrap();
        let w = db.get_work_item("old1").unwrap().expect("row kept");
        assert_eq!(
            (
                w.title.as_str(),
                w.notes.as_str(),
                w.state.as_str(),
                w.position,
                w.created_at,
                w.updated_at
            ),
            ("Old title", "old notes", "blocked", 4, 10, 11)
        );
        assert_eq!(
            (
                w.due_at,
                w.waiting_since,
                w.completed_at,
                w.delegated_to_contact_id
            ),
            (None, None, None, None)
        );
        assert_eq!(db.work_items_for(EntityKind::Thread, "t").unwrap().len(), 1);
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
    }

    // ---- AW3: relative dates and named shorthands ------------------------

    const DAY: i64 = 86_400;
    /// A UTC midnight with a known weekday: Wed 2027-01-13.
    const WED: i64 = 1_799_798_400;

    fn list_tz(db: &Database, json: &str, now: i64, tz: i64) -> Vec<String> {
        let mut t = titles(
            &db.list_work_items_filtered_tz(&filter(json), now, tz)
                .unwrap(),
        );
        t.sort();
        t
    }

    fn seed_relative(db: &Database) {
        let p = project(db);
        mk(db, Some(&p), "yesterday", "needs_reply", Some(WED - 3600));
        mk(db, Some(&p), "today-early", "waiting", Some(WED + 60));
        mk(db, Some(&p), "today-late", "delegated", Some(WED + DAY - 1));
        mk(db, Some(&p), "tomorrow", "needs_reply", Some(WED + DAY));
        mk(
            db,
            Some(&p),
            "friday",
            "needs_reply",
            Some(WED + 2 * DAY + 5),
        );
        mk(
            db,
            Some(&p),
            "sunday",
            "needs_reply",
            Some(WED + 4 * DAY + 5),
        );
        mk(db, Some(&p), "next-mon", "needs_reply", Some(WED + 5 * DAY));
        mk(db, Some(&p), "last-sun", "needs_reply", Some(WED - 3 * DAY));
        mk(db, Some(&p), "undated", "needs_reply", None);
        mk(db, Some(&p), "done-old", "done", Some(WED - DAY));
    }

    #[test]
    fn due_today_this_week_and_overdue_are_relative_to_the_clock() {
        let db = fresh_db();
        seed_relative(&db);
        let noon = WED + 12 * 3600;
        assert_eq!(
            list_tz(&db, r#"{"due":["today"]}"#, noon, 0),
            ["today-early", "today-late"]
        );
        // Week = Mon 2027-01-11 .. Sun 2027-01-17 inclusive.
        assert_eq!(
            list_tz(&db, r#"{"due":["this_week"]}"#, noon, 0),
            [
                "done-old",
                "friday",
                "sunday",
                "today-early",
                "today-late",
                "tomorrow",
                "yesterday"
            ]
        );
        // overdue keeps its AW1 meaning: due before now, not done/someday.
        assert_eq!(
            list_tz(&db, r#"{"due":["overdue"]}"#, noon, 0),
            ["last-sun", "today-early", "yesterday"]
        );
        // Several windows are OR-ed; other keys still AND.
        assert_eq!(
            list_tz(&db, r#"{"due":["overdue","today"]}"#, noon, 0),
            ["last-sun", "today-early", "today-late", "yesterday"]
        );
        assert_eq!(
            list_tz(
                &db,
                r#"{"due":["overdue","today"],"states":["delegated","waiting"]}"#,
                noon,
                0
            ),
            ["today-early", "today-late"]
        );
        assert!(list_tz(&db, r#"{"due":[]}"#, noon, 0).is_empty());
        // An unknown window is a parse error, never ignored.
        assert!(serde_json::from_str::<WorkItemFilter>(r#"{"due":["soon"]}"#).is_err());
    }

    #[test]
    fn relative_dates_follow_the_local_offset() {
        let db = fresh_db();
        let p = project(&db);
        // 23:30 UTC Wed is already Thu 00:30 at UTC+1.
        mk(
            &db,
            Some(&p),
            "wed-2330-utc",
            "needs_reply",
            Some(WED + 23 * 3600 + 1800),
        );
        let noon = WED + 12 * 3600;
        let today = r#"{"due":["today"]}"#;
        assert_eq!(list_tz(&db, today, noon, 0), ["wed-2330-utc"]);
        assert!(list_tz(&db, today, noon, 3600).is_empty());
        // Thu 01:00 UTC is Thu 02:00 local at UTC+1: the item is due today.
        assert_eq!(
            list_tz(&db, today, WED + DAY + 3600, 3600),
            ["wed-2330-utc"]
        );
    }

    #[test]
    fn a_clock_crossing_midnight_moves_items_between_today_and_overdue() {
        let db = fresh_db();
        let p = project(&db);
        mk(
            &db,
            Some(&p),
            "due-at-midnight",
            "needs_reply",
            Some(WED + DAY),
        );
        mk(
            &db,
            Some(&p),
            "due-wed-evening",
            "needs_reply",
            Some(WED + 20 * 3600),
        );
        let today_view = r#"{"due":["overdue","today"]}"#;
        let overdue = r#"{"due":["overdue"]}"#;
        let just_before = WED + DAY - 1; // Wed 23:59:59
        let just_after = WED + DAY; // Thu 00:00:00
        assert_eq!(
            list_tz(&db, today_view, just_before, 0),
            ["due-wed-evening"]
        );
        assert_eq!(list_tz(&db, overdue, just_before, 0), ["due-wed-evening"]);
        // After midnight the evening item is overdue, the midnight item is
        // due today (due_at == now is not yet overdue).
        assert_eq!(
            list_tz(&db, r#"{"due":["today"]}"#, just_after, 0),
            ["due-at-midnight"]
        );
        assert_eq!(list_tz(&db, overdue, just_after, 0), ["due-wed-evening"]);
        assert_eq!(
            list_tz(&db, today_view, just_after, 0),
            ["due-at-midnight", "due-wed-evening"]
        );
        // Sunday 23:59:59 -> Monday 00:00:00 rolls the week over.
        mk(
            &db,
            Some(&p),
            "mon-9am",
            "needs_reply",
            Some(WED + 5 * DAY + 9 * 3600),
        );
        let week = r#"{"due":["this_week"]}"#;
        assert!(!list_tz(&db, week, WED + 5 * DAY - 1, 0).contains(&"mon-9am".to_string()));
        assert_eq!(list_tz(&db, week, WED + 5 * DAY, 0), ["mon-9am"]);
    }

    #[test]
    fn waiting_shorthands_map_to_states() {
        let db = fresh_db();
        let p = project(&db);
        for (t, s) in [
            ("nr", "needs_reply"),
            ("w", "waiting"),
            ("d", "delegated"),
            ("b", "blocked"),
            ("x", "done"),
            ("s", "someday"),
        ] {
            mk(&db, Some(&p), t, s, None);
        }
        assert_eq!(list_tz(&db, r#"{"waiting_on_me":true}"#, NOW, 0), ["nr"]);
        assert_eq!(
            list_tz(&db, r#"{"waiting_on_me":false}"#, NOW, 0),
            ["b", "d", "s", "w", "x"]
        );
        assert_eq!(
            list_tz(&db, r#"{"waiting_on_others":true}"#, NOW, 0),
            ["d", "w"]
        );
        assert_eq!(
            list_tz(&db, r#"{"waiting_on_others":false}"#, NOW, 0),
            ["b", "nr", "s", "x"]
        );
        assert_eq!(
            list_tz(
                &db,
                r#"{"waiting_on_others":true,"states":["waiting"]}"#,
                NOW,
                0
            ),
            ["w"]
        );
    }
}
