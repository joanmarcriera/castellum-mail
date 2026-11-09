// Castellum - project activity + decision timeline (Active Work backend).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// An append-only log per project (migration 0049). Each row is a `kind`
// (activity | decision | work_item_change), a short human `summary`, an
// optional typed reference (`ref_type` + `ref_id`, ids only: never a message
// body or subject) and `created_at`. The table forbids UPDATE and forbids
// DELETE except through the cascade of deleting the project, so the history
// of "what happened and what we decided" cannot be rewritten.
//
// Entries are appended by explicit calls (`add_timeline_entry`,
// `record_decision`) and by hooks inside this crate: a work item's creation,
// state change or deletion (work_items.rs), a message being linked to or
// unlinked from a project (projects.rs) and a message being attached to a
// work item. Every append bumps the project's `last_activity_at` (never
// backwards). Listing is newest first with limit/offset paging.

use crate::{Database, DbError, EntityKind};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// Longest stored summary, in characters (longer input is truncated).
pub const TIMELINE_SUMMARY_MAX_CHARS: usize = 500;
/// Largest page `list_timeline` will return.
pub const TIMELINE_MAX_LIMIT: i64 = 500;

/// What an entry records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineKind {
    /// Something happened (a message was linked, ...).
    Activity,
    /// A decision the user recorded.
    Decision,
    /// A work item was created, changed state or deleted.
    WorkItemChange,
}

impl TimelineKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TimelineKind::Activity => "activity",
            TimelineKind::Decision => "decision",
            TimelineKind::WorkItemChange => "work_item_change",
        }
    }

    /// Inverse of `as_str`; `None` for unknown kinds (callers must reject).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "activity" => Some(TimelineKind::Activity),
            "decision" => Some(TimelineKind::Decision),
            "work_item_change" => Some(TimelineKind::WorkItemChange),
            _ => None,
        }
    }
}

/// One stored timeline row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TimelineEntry {
    pub id: i64,
    pub project_id: String,
    /// `activity` | `decision` | `work_item_change`.
    pub kind: String,
    pub summary: String,
    /// `message` | `thread` | `contact` | `work_item`, or None.
    pub ref_type: Option<String>,
    pub ref_id: Option<String>,
    pub created_at: i64,
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Trim and cap a summary; empty after trimming is an error.
fn clean_summary(summary: &str) -> Result<String, DbError> {
    let t = summary.trim();
    if t.is_empty() {
        return Err(DbError("timeline summary must not be empty".into()));
    }
    Ok(t.chars().take(TIMELINE_SUMMARY_MAX_CHARS).collect())
}

/// Shorten a user-owned title for use inside a hook-written summary.
pub(crate) fn short(title: &str) -> String {
    let t = title.trim();
    if t.chars().count() > 80 {
        format!("{}...", t.chars().take(80).collect::<String>())
    } else {
        t.to_string()
    }
}

/// Append one row on an existing connection/transaction and bump the
/// project's `last_activity_at`. Shared by the public API and the hooks in
/// work_items.rs / projects.rs, which already hold the connection lock.
pub(crate) fn insert_entry(
    conn: &Connection,
    project_id: &str,
    kind: TimelineKind,
    summary: &str,
    reference: Option<(EntityKind, &str)>,
    now: i64,
) -> Result<TimelineEntry, DbError> {
    let summary = clean_summary(summary)?;
    if let Some((EntityKind::Project, _)) = reference {
        return Err(DbError(
            "a timeline reference must be a message, thread, contact or work item".into(),
        ));
    }
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM projects WHERE id = ?1",
            params![project_id],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_none() {
        return Err(DbError(format!("project {project_id} not found")));
    }
    let (ref_type, ref_id) = match reference {
        Some((k, id)) => (Some(k.as_str()), Some(id)),
        None => (None, None),
    };
    conn.execute(
        "INSERT INTO project_timeline (project_id, kind, summary, ref_type, ref_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![project_id, kind.as_str(), summary, ref_type, ref_id, now],
    )?;
    let id = conn.last_insert_rowid();
    conn.execute(
        "UPDATE projects SET last_activity_at = MAX(last_activity_at, ?2) WHERE id = ?1",
        params![project_id, now],
    )?;
    Ok(TimelineEntry {
        id,
        project_id: project_id.to_string(),
        kind: kind.as_str().to_string(),
        summary,
        ref_type: ref_type.map(str::to_string),
        ref_id: ref_id.map(str::to_string),
        created_at: now,
    })
}

fn row_to_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<TimelineEntry> {
    Ok(TimelineEntry {
        id: r.get(0)?,
        project_id: r.get(1)?,
        kind: r.get(2)?,
        summary: r.get(3)?,
        ref_type: r.get(4)?,
        ref_id: r.get(5)?,
        created_at: r.get(6)?,
    })
}

impl Database {
    /// Append an entry at the current time. See [`Database::add_timeline_entry_at`].
    pub fn add_timeline_entry(
        &self,
        project_id: &str,
        kind: TimelineKind,
        summary: &str,
        reference: Option<(EntityKind, &str)>,
    ) -> Result<TimelineEntry, DbError> {
        self.add_timeline_entry_at(project_id, kind, summary, reference, now_epoch())
    }

    /// Append an entry stamped `now`. The project must exist; the summary
    /// is trimmed, must be non-empty and is capped at
    /// [`TIMELINE_SUMMARY_MAX_CHARS`]; a reference is ids only and may not
    /// point at a project. Bumps the project's `last_activity_at`.
    pub fn add_timeline_entry_at(
        &self,
        project_id: &str,
        kind: TimelineKind,
        summary: &str,
        reference: Option<(EntityKind, &str)>,
        now: i64,
    ) -> Result<TimelineEntry, DbError> {
        let conn = self.lock_conn();
        insert_entry(&conn, project_id, kind, summary, reference, now)
    }

    /// Record a user decision (`kind = decision`) at the current time.
    pub fn record_decision(
        &self,
        project_id: &str,
        summary: &str,
        reference: Option<(EntityKind, &str)>,
    ) -> Result<TimelineEntry, DbError> {
        self.add_timeline_entry(project_id, TimelineKind::Decision, summary, reference)
    }

    /// A project's timeline, newest first (created_at desc, id desc).
    /// `kinds` (None or empty = all kinds) filters; `limit` is clamped to
    /// 1..=[`TIMELINE_MAX_LIMIT`] and `offset` to >= 0. An unknown project
    /// yields an empty page.
    pub fn list_timeline(
        &self,
        project_id: &str,
        kinds: Option<&[TimelineKind]>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TimelineEntry>, DbError> {
        let limit = limit.clamp(1, TIMELINE_MAX_LIMIT);
        let offset = offset.max(0);
        let kinds: Vec<&str> = kinds.unwrap_or(&[]).iter().map(|k| k.as_str()).collect();
        let mut sql = String::from(
            "SELECT id, project_id, kind, summary, ref_type, ref_id, created_at
             FROM project_timeline WHERE project_id = ?1",
        );
        if !kinds.is_empty() {
            let marks = (0..kinds.len())
                .map(|i| format!("?{}", i + 2))
                .collect::<Vec<_>>()
                .join(",");
            sql.push_str(&format!(" AND kind IN ({marks})"));
        }
        let n = kinds.len();
        sql.push_str(&format!(
            " ORDER BY created_at DESC, id DESC LIMIT ?{} OFFSET ?{}",
            n + 2,
            n + 3
        ));
        let mut args: Vec<rusqlite::types::Value> = vec![project_id.to_string().into()];
        args.extend(
            kinds
                .iter()
                .map(|k| rusqlite::types::Value::from(k.to_string())),
        );
        args.push(limit.into());
        args.push(offset.into());
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(args), row_to_entry)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Number of timeline rows for a project (all kinds).
    pub fn count_timeline(&self, project_id: &str) -> Result<i64, DbError> {
        let conn = self.lock_conn();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM project_timeline WHERE project_id = ?1",
            params![project_id],
            |r| r.get(0),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProjectUpsert, WorkItemUpsert};
    use castellum_models::{Account, Mailbox, NewMessage};
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn fresh_db() -> Database {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-timeline-test-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        Database::open(&path).expect("open")
    }

    fn project(db: &Database) -> String {
        db.upsert_project(ProjectUpsert {
            name: "P",
            status: "active",
            position: 0,
            ..Default::default()
        })
        .unwrap()
        .id
    }

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
            subject: Some("SECRET SUBJECT".into()),
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
            text_body: Some("SECRET BODY".into()),
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: Vec::new(),
        })
        .unwrap();
    }

    fn all(db: &Database, pid: &str) -> Vec<TimelineEntry> {
        db.list_timeline(pid, None, 500, 0).unwrap()
    }

    #[test]
    fn add_list_newest_first_and_decision_kind() {
        let db = fresh_db();
        let pid = project(&db);
        db.add_timeline_entry_at(&pid, TimelineKind::Activity, "first", None, 100)
            .unwrap();
        let d = db
            .record_decision(&pid, "  Go with vendor B  ", None)
            .unwrap();
        assert_eq!(d.kind, "decision");
        assert_eq!(d.summary, "Go with vendor B");
        db.add_timeline_entry_at(&pid, TimelineKind::Activity, "second", None, 200)
            .unwrap();
        let rows = all(&db, &pid);
        assert_eq!(rows.len(), 3);
        // The decision was stamped "now" (>> 200), so it sorts first.
        assert_eq!(rows[0].summary, "Go with vendor B");
        assert_eq!(rows[1].summary, "second");
        assert_eq!(rows[2].summary, "first");
        assert_eq!(db.count_timeline(&pid).unwrap(), 3);
    }

    #[test]
    fn equal_timestamps_order_by_id_desc() {
        let db = fresh_db();
        let pid = project(&db);
        for s in ["a", "b", "c"] {
            db.add_timeline_entry_at(&pid, TimelineKind::Activity, s, None, 500)
                .unwrap();
        }
        let got: Vec<_> = all(&db, &pid).into_iter().map(|e| e.summary).collect();
        assert_eq!(got, ["c", "b", "a"]);
    }

    #[test]
    fn paging_and_kind_filter() {
        let db = fresh_db();
        let pid = project(&db);
        for i in 0..7 {
            let k = if i % 2 == 0 {
                TimelineKind::Activity
            } else {
                TimelineKind::Decision
            };
            db.add_timeline_entry_at(&pid, k, &format!("e{i}"), None, 1000 + i)
                .unwrap();
        }
        let p1 = db.list_timeline(&pid, None, 3, 0).unwrap();
        let p2 = db.list_timeline(&pid, None, 3, 3).unwrap();
        let p3 = db.list_timeline(&pid, None, 3, 6).unwrap();
        let p4 = db.list_timeline(&pid, None, 3, 9).unwrap();
        let names = |v: &[TimelineEntry]| v.iter().map(|e| e.summary.clone()).collect::<Vec<_>>();
        assert_eq!(names(&p1), ["e6", "e5", "e4"]);
        assert_eq!(names(&p2), ["e3", "e2", "e1"]);
        assert_eq!(names(&p3), ["e0"]);
        assert!(p4.is_empty());
        let dec = db
            .list_timeline(&pid, Some(&[TimelineKind::Decision]), 50, 0)
            .unwrap();
        assert_eq!(names(&dec), ["e5", "e3", "e1"]);
        let both = db
            .list_timeline(
                &pid,
                Some(&[TimelineKind::Decision, TimelineKind::Activity]),
                50,
                0,
            )
            .unwrap();
        assert_eq!(both.len(), 7);
        // Empty kinds slice = no filter; limit/offset are clamped, not errors.
        assert_eq!(db.list_timeline(&pid, Some(&[]), 0, -5).unwrap().len(), 1);
        assert_eq!(db.list_timeline(&pid, None, 10_000, 0).unwrap().len(), 7);
    }

    #[test]
    fn validation_errors() {
        let db = fresh_db();
        let pid = project(&db);
        assert!(db
            .add_timeline_entry(&pid, TimelineKind::Activity, "   ", None)
            .is_err());
        assert!(db
            .add_timeline_entry("nope", TimelineKind::Activity, "x", None)
            .is_err());
        assert!(db
            .add_timeline_entry(
                &pid,
                TimelineKind::Activity,
                "x",
                Some((EntityKind::Project, &pid))
            )
            .is_err());
        assert_eq!(db.count_timeline(&pid).unwrap(), 0);
        assert!(db.list_timeline("nope", None, 10, 0).unwrap().is_empty());
        let long = "é".repeat(TIMELINE_SUMMARY_MAX_CHARS + 50);
        let e = db
            .add_timeline_entry(&pid, TimelineKind::Activity, &long, None)
            .unwrap();
        assert_eq!(e.summary.chars().count(), TIMELINE_SUMMARY_MAX_CHARS);
        assert_eq!(
            TimelineKind::parse("decision"),
            Some(TimelineKind::Decision)
        );
        assert_eq!(TimelineKind::parse("bogus"), None);
    }

    #[test]
    fn append_bumps_last_activity_but_never_backwards() {
        let db = fresh_db();
        let pid = project(&db);
        let before = db
            .get_project_hydrated(&pid)
            .unwrap()
            .unwrap()
            .project
            .last_activity_at;
        db.add_timeline_entry_at(&pid, TimelineKind::Activity, "future", None, before + 5000)
            .unwrap();
        let after = db
            .get_project_hydrated(&pid)
            .unwrap()
            .unwrap()
            .project
            .last_activity_at;
        assert_eq!(after, before + 5000);
        db.add_timeline_entry_at(&pid, TimelineKind::Activity, "old", None, 10)
            .unwrap();
        assert_eq!(
            db.get_project_hydrated(&pid)
                .unwrap()
                .unwrap()
                .project
                .last_activity_at,
            after
        );
    }

    #[test]
    fn rows_are_append_only_but_cascade_with_the_project() {
        let db = fresh_db();
        let pid = project(&db);
        let e = db.record_decision(&pid, "keep", None).unwrap();
        {
            let conn = db.lock_conn();
            assert!(conn
                .execute(
                    "UPDATE project_timeline SET summary = 'x' WHERE id = ?1",
                    params![e.id]
                )
                .is_err());
            assert!(conn
                .execute("DELETE FROM project_timeline WHERE id = ?1", params![e.id])
                .is_err());
        }
        assert_eq!(db.count_timeline(&pid).unwrap(), 1);
        db.delete_project(&pid).unwrap();
        assert_eq!(db.count_timeline(&pid).unwrap(), 0);
    }

    #[test]
    fn work_item_hooks_record_create_state_change_and_delete() {
        let db = fresh_db();
        let pid = project(&db);
        let w = db
            .upsert_work_item(WorkItemUpsert {
                project_id: Some(&pid),
                title: "Chase invoice",
                state: "needs_reply",
                ..Default::default()
            })
            .unwrap();
        // Same state + new notes: no entry.
        db.upsert_work_item(WorkItemUpsert {
            id: Some(&w.id),
            project_id: Some(&pid),
            title: "Chase invoice",
            notes: "edited",
            state: "needs_reply",
            ..Default::default()
        })
        .unwrap();
        assert_eq!(db.count_timeline(&pid).unwrap(), 1);
        db.upsert_work_item(WorkItemUpsert {
            id: Some(&w.id),
            project_id: Some(&pid),
            title: "Chase invoice",
            state: "waiting",
            ..Default::default()
        })
        .unwrap();
        db.delete_work_item(&w.id).unwrap();
        let rows = all(&db, &pid);
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r.kind == "work_item_change"));
        assert!(rows
            .iter()
            .all(|r| r.ref_type.as_deref() == Some("work_item")
                && r.ref_id.as_deref() == Some(w.id.as_str())));
        assert!(rows[2].summary.contains("created"));
        assert!(rows[1].summary.contains("needs_reply") && rows[1].summary.contains("waiting"));
        assert!(rows[0].summary.contains("deleted"));
    }

    #[test]
    fn unassigned_work_items_write_no_timeline() {
        let db = fresh_db();
        let pid = project(&db);
        let w = db
            .upsert_work_item(WorkItemUpsert {
                project_id: None,
                title: "loose",
                state: "needs_reply",
                ..Default::default()
            })
            .unwrap();
        db.delete_work_item(&w.id).unwrap();
        assert_eq!(db.count_timeline(&pid).unwrap(), 0);
        let conn = db.lock_conn();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM project_timeline", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn message_link_and_unlink_hooks_are_idempotent_and_carry_no_content() {
        let db = fresh_db();
        let pid = project(&db);
        seed_message(&db, "m1");
        db.set_email_link(&pid, "m1", true, false).unwrap();
        db.set_email_link(&pid, "m1", true, false).unwrap(); // repeat: no new row
        assert_eq!(db.count_timeline(&pid).unwrap(), 1);
        db.set_email_link(&pid, "m1", false, false).unwrap();
        db.set_email_link(&pid, "m1", false, false).unwrap(); // nothing to unlink
        let rows = all(&db, &pid);
        assert_eq!(rows.len(), 2);
        assert!(rows[1].summary.contains("linked") && rows[0].summary.contains("unlinked"));
        for r in &rows {
            assert_eq!(r.kind, "activity");
            assert_eq!(r.ref_type.as_deref(), Some("message"));
            assert_eq!(r.ref_id.as_deref(), Some("m1"));
            assert!(!r.summary.contains("SECRET"));
        }
    }

    #[test]
    fn linking_a_message_to_a_work_item_records_on_its_project() {
        let db = fresh_db();
        let pid = project(&db);
        seed_message(&db, "m1");
        let w = db
            .upsert_work_item(WorkItemUpsert {
                project_id: Some(&pid),
                title: "T",
                state: "needs_reply",
                ..Default::default()
            })
            .unwrap();
        db.link_work_item(&w.id, EntityKind::Message, "m1").unwrap();
        db.link_work_item(&w.id, EntityKind::Message, "m1").unwrap(); // idempotent
        let rows = all(&db, &pid);
        assert_eq!(rows.len(), 2); // created + one link
        assert_eq!(rows[0].ref_type.as_deref(), Some("message"));
        assert_eq!(rows[0].ref_id.as_deref(), Some("m1"));
        assert_eq!(rows[0].kind, "work_item_change");
        // Unassigned items and non-message targets add nothing.
        let loose = db
            .upsert_work_item(WorkItemUpsert {
                project_id: None,
                title: "L",
                state: "needs_reply",
                ..Default::default()
            })
            .unwrap();
        db.link_work_item(&loose.id, EntityKind::Message, "m1")
            .unwrap();
        db.link_work_item(&w.id, EntityKind::Thread, "t1").unwrap();
        assert_eq!(db.count_timeline(&pid).unwrap(), 2);
    }

    #[test]
    fn failed_upsert_writes_no_timeline_row() {
        let db = fresh_db();
        let pid = project(&db);
        assert!(db
            .upsert_work_item(WorkItemUpsert {
                id: Some("missing"),
                project_id: Some(&pid),
                title: "T",
                state: "waiting",
                ..Default::default()
            })
            .is_err());
        assert_eq!(db.count_timeline(&pid).unwrap(), 0);
    }
}
