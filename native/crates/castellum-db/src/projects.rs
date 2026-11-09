// Castellum - projects persistence layer (slice 8b).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// All methods on `Database`. Project ids are uuid-v4 strings. Links between
// projects and emails/contacts are idempotent; linking a message with
// `auto_contacts=true` inserts the message's non-self, non-hidden from/to/cc
// participants as origin='auto' contacts, but never downgrades an existing
// 'manual' row to 'auto'. `move_project` is atomic: it updates the target
// project's status/position and re-packs all positions in both affected
// columns to keep them contiguous.

use crate::project_timeline::{insert_entry, TimelineKind};
use crate::{Database, DbError, EntityKind};
use castellum_models::{
    MessageHeader, Project, ProjectCard, ProjectContactLink, ProjectDetail, ProjectTag,
};
use rusqlite::{params, params_from_iter, OptionalExtension};
use std::collections::HashMap;
use uuid::Uuid;

/// Unix epoch seconds now (fallback to 0 on a pre-1970 clock).
fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Collect a list of id strings from a prepared statement within a transaction.
/// Using a standalone function lets the borrow on `stmt` end before the caller
/// uses the returned `Vec`, avoiding lifetime issues with block expressions.
fn collect_ids(
    tx: &rusqlite::Transaction<'_>,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<String>, DbError> {
    let mut stmt = tx.prepare(sql)?;
    let ids = stmt
        .query_map(params, |r| r.get(0))?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(ids)
}

/// Row mapper for the standard message-header projection (13 columns).
fn row_to_header(r: &rusqlite::Row<'_>) -> rusqlite::Result<MessageHeader> {
    Ok(MessageHeader {
        id: r.get(0)?,
        account_id: r.get(1)?,
        mailbox_id: r.get(2)?,
        subject: r.get(3)?,
        from_name: r.get(4)?,
        from_address: r.get(5)?,
        sent_at: r.get(6)?,
        snippet: r.get(7)?,
        has_attachments: r.get(8)?,
        is_read: r.get(9)?,
        thread_count: r.get(10)?,
        is_starred: r.get(11)?,
        has_body: r.get(12)?,
        keywords: Vec::new(),
        thread_muted: r.get(13)?,
        from_vip: r.get(14)?,
        to_display: None,
    })
}

/// Parameters for [`Database::upsert_project`]. Grouping the fields keeps the
/// call sites self-documenting (and the argument count within clippy's limit).
/// `Default` gives `id: None, notes: "", color: None, position: 0,
/// archived: None` — callers set `name`/`status` and use struct-update syntax.
#[derive(Debug, Default)]
pub struct ProjectUpsert<'a> {
    /// Existing project id; `None`/empty creates a new project (uuid-v4).
    pub id: Option<&'a str>,
    pub name: &'a str,
    pub notes: &'a str,
    pub status: &'a str,
    pub color: Option<&'a str>,
    pub position: i64,
    /// `Some(_)` writes the flag; `None` leaves the stored value unchanged
    /// (create path defaults to not-archived).
    pub archived: Option<bool>,
}

impl Database {
    /// Create (when `id` is empty/absent) or update a project. On create a
    /// uuid-v4 is generated. Returns the stored `Project` on success.
    pub fn upsert_project(&self, p: ProjectUpsert<'_>) -> Result<Project, DbError> {
        let ProjectUpsert {
            id,
            name,
            notes,
            status,
            color,
            position,
            archived,
        } = p;
        if name.trim().is_empty() {
            return Err(DbError("project name must not be empty".into()));
        }
        let now = now_epoch();
        let conn = self.lock_conn();

        if let Some(id) = id.filter(|s| !s.is_empty()) {
            // Update path. `archived` is only written when explicitly supplied
            // (COALESCE keeps the stored value on None).
            let n = conn.execute(
                "UPDATE projects
                 SET name = ?2, notes = ?3, status = ?4, color = ?5,
                     position = ?6, updated_at = ?7, last_activity_at = ?7,
                     archived = COALESCE(?8, archived)
                 WHERE id = ?1",
                params![
                    id,
                    name,
                    notes,
                    status,
                    color,
                    position,
                    now,
                    archived.map(i64::from)
                ],
            )?;
            if n == 0 {
                // Id supplied but not found — treat as create with that id.
                conn.execute(
                    "INSERT INTO projects (id, name, notes, status, color, position,
                                          archived, created_at, updated_at, last_activity_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?8, ?7, ?7, ?7)",
                    params![
                        id,
                        name,
                        notes,
                        status,
                        color,
                        position,
                        now,
                        i64::from(archived.unwrap_or(false))
                    ],
                )?;
            }
            Ok(conn.query_row(
                "SELECT id, name, notes, status, color, position, archived,
                        created_at, updated_at, last_activity_at
                 FROM projects WHERE id = ?1",
                params![id],
                row_to_project,
            )?)
        } else {
            // Create path.
            let new_id = Uuid::new_v4().to_string();
            conn.execute(
                "INSERT INTO projects (id, name, notes, status, color, position,
                                      archived, created_at, updated_at, last_activity_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?8, ?7, ?7, ?7)",
                params![
                    new_id,
                    name,
                    notes,
                    status,
                    color,
                    position,
                    now,
                    i64::from(archived.unwrap_or(false))
                ],
            )?;
            Ok(conn.query_row(
                "SELECT id, name, notes, status, color, position, archived,
                        created_at, updated_at, last_activity_at
                 FROM projects WHERE id = ?1",
                params![new_id],
                row_to_project,
            )?)
        }
    }

    /// List projects with per-project email and contact counts, ordered by
    /// `(status, position)`. Archived projects are excluded unless
    /// `include_archived` is true.
    pub fn list_projects_with_counts(
        &self,
        include_archived: bool,
    ) -> Result<Vec<ProjectCard>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT p.id, p.name, p.notes, p.status, p.color, p.position, p.archived,
                    p.created_at, p.updated_at, p.last_activity_at,
                    COUNT(DISTINCT pe.dst_id) AS email_count,
                    COUNT(DISTINCT pc.dst_id) AS contact_count
             FROM projects p
             LEFT JOIN links pe ON pe.src_type = 'project' AND pe.src_id = p.id
                               AND pe.relation = 'includes' AND pe.dst_type = 'message'
             LEFT JOIN links pc ON pc.src_type = 'project' AND pc.src_id = p.id
                               AND pc.relation = 'involves' AND pc.dst_type = 'contact'
             WHERE (p.archived = 0 OR ?1)
             GROUP BY p.id
             ORDER BY CASE p.status
                          WHEN 'active'   THEN 0
                          WHEN 'waiting'  THEN 1
                          WHEN 'someday'  THEN 2
                          WHEN 'done'     THEN 3
                          ELSE 4 END,
                      p.position, p.name",
        )?;
        let rows = stmt.query_map(params![include_archived], row_to_card)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Fetch a project with all its linked message headers and contact links.
    /// Returns `None` when the project id is unknown.
    pub fn get_project_hydrated(&self, id: &str) -> Result<Option<ProjectDetail>, DbError> {
        let conn = self.lock_conn();

        // Project row.
        let project = conn
            .query_row(
                "SELECT id, name, notes, status, color, position, archived,
                        created_at, updated_at, last_activity_at
                 FROM projects WHERE id = ?1",
                params![id],
                row_to_project,
            )
            .optional()?;
        let project = match project {
            Some(p) => p,
            None => return Ok(None),
        };

        // Linked message headers (same projection as list_message_headers).
        let mut hstmt = conn.prepare(
            "SELECT m.id, m.account_id, m.mailbox_id, m.subject, m.from_name, m.from_address,
                    m.sent_at, m.snippet, m.has_attachments,
                    COALESCE(f.is_read, 0), COALESCE(t.message_count, 1),
                    COALESCE(f.is_starred, 0), b.message_id IS NOT NULL,
                    COALESCE(t.is_muted, 0), EXISTS(SELECT 1 FROM vip_senders v WHERE v.address_key = lower(trim(m.from_address)))
             FROM links pe
             JOIN messages m ON m.id = pe.dst_id
             LEFT JOIN message_flags f ON f.message_id = m.id
             LEFT JOIN threads t ON t.id = m.thread_id
             LEFT JOIN message_bodies b ON b.message_id = m.id
             WHERE pe.src_type = 'project' AND pe.src_id = ?1
               AND pe.relation = 'includes' AND pe.dst_type = 'message'
             ORDER BY m.sent_at DESC, m.id",
        )?;
        let mut emails: Vec<MessageHeader> = hstmt
            .query_map(params![id], row_to_header)?
            .collect::<Result<Vec<_>, _>>()?;

        // S42: keywords, via the SAME held `conn` — never `self.
        // attach_keywords`, which would try to re-lock and deadlock (this
        // method holds `conn` for its whole body).
        if !emails.is_empty() {
            let placeholders = vec!["?"; emails.len()].join(",");
            let mut kstmt = conn.prepare(&format!(
                "SELECT message_id, keyword FROM message_keywords WHERE message_id IN ({placeholders})"
            ))?;
            let ids: Vec<&str> = emails.iter().map(|h| h.id.as_str()).collect();
            let mut kw_map: HashMap<String, Vec<String>> = HashMap::new();
            let rows = kstmt.query_map(params_from_iter(ids.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (mid, kw) = row?;
                kw_map.entry(mid).or_default().push(kw);
            }
            for h in emails.iter_mut() {
                if let Some(kw) = kw_map.remove(&h.id) {
                    h.keywords = kw
                        .into_iter()
                        .filter(|k| !crate::tags::is_hidden_keyword(k))
                        .collect();
                }
            }
        }

        // Linked contacts with origin. Stored origin 'derived' renders as the
        // legacy DTO value "auto" (FFI/Swift shape unchanged by 0017).
        let mut cstmt = conn.prepare(
            "SELECT CAST(pc.dst_id AS INTEGER), c.email, c.derived_name, c.display_name,
                    CASE pc.origin WHEN 'derived' THEN 'auto' ELSE pc.origin END,
                    pc.updated_at
             FROM links pc
             JOIN contacts c ON c.id = CAST(pc.dst_id AS INTEGER)
             WHERE pc.src_type = 'project' AND pc.src_id = ?1
               AND pc.relation = 'involves' AND pc.dst_type = 'contact'
             ORDER BY CASE pc.origin WHEN 'derived' THEN 'auto' ELSE pc.origin END DESC,
                      c.email",
        )?;
        let contacts: Vec<ProjectContactLink> = cstmt
            .query_map(params![id], |r| {
                Ok(ProjectContactLink {
                    contact_id: r.get(0)?,
                    email: r.get(1)?,
                    derived_name: r.get(2)?,
                    display_name: r.get(3)?,
                    origin: r.get(4)?,
                    linked_at: r.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Some(ProjectDetail {
            project,
            emails,
            contacts,
        }))
    }

    /// Permanently delete a project (cascade drops all link rows).
    pub fn delete_project(&self, id: &str) -> Result<(), DbError> {
        let conn = self.lock_conn();
        let n = conn.execute("DELETE FROM projects WHERE id = ?1", params![id])?;
        if n == 0 {
            return Err(DbError(format!("project {id} not found")));
        }
        Ok(())
    }

    /// Link or unlink a message from a project. Idempotent (link when already
    /// linked / unlink when not linked are no-ops). When `linked` is true and
    /// `auto_contacts` is true, the message's from/to/cc participants that are
    /// NOT `is_self` and NOT `is_hidden` become derived `links` rows
    /// (relation 'involves', provenance = the evidencing message id, rendered
    /// as origin "auto" in the DTO); existing manual rows for those contacts
    /// are never downgraded. Bumps `last_activity_at` when `linked`.
    pub fn set_email_link(
        &self,
        project_id: &str,
        message_id: &str,
        linked: bool,
        auto_contacts: bool,
    ) -> Result<(), DbError> {
        let now = now_epoch();
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;

        // Timeline hook input: was the message already linked?
        let was_linked: bool = tx
            .query_row(
                "SELECT 1 FROM links
                 WHERE src_type = 'project' AND src_id = ?1
                   AND dst_type = 'message' AND dst_id = ?2
                   AND relation = 'includes'",
                params![project_id, message_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();

        if linked {
            // Full idempotent-upsert contract: a repeated manual link bumps
            // updated_at while preserving any note (created_at stays).
            tx.execute(
                "INSERT INTO links
                     (src_type, src_id, dst_type, dst_id, relation, origin,
                      provenance, confidence, note, created_at, updated_at)
                 VALUES ('project', ?1, 'message', ?2, 'includes', 'manual',
                         NULL, NULL, NULL, ?3, ?3)
                 ON CONFLICT(src_type, src_id, dst_type, dst_id, relation)
                 DO UPDATE SET origin = 'manual', updated_at = excluded.updated_at",
                params![project_id, message_id, now],
            )?;
            tx.execute(
                "UPDATE projects SET last_activity_at = ?2 WHERE id = ?1",
                params![project_id, now],
            )?;

            if auto_contacts {
                // Collect non-self, non-hidden from/to/cc participants as
                // derived links whose provenance names the evidencing message
                // ("why is this connected?" = it participated in this mail).
                // A derived refresh updates the evidence to the latest
                // message; manual rows are never downgraded or touched.
                tx.execute(
                    "INSERT INTO links
                         (src_type, src_id, dst_type, dst_id, relation, origin,
                          provenance, confidence, note, created_at, updated_at)
                     SELECT 'project', ?1, 'contact', CAST(c.id AS TEXT),
                            'involves', 'derived', 'message:' || ?2, NULL, NULL,
                            ?3, ?3
                     FROM message_addresses ma
                     JOIN contacts c ON c.email = lower(trim(ma.address))
                     WHERE ma.message_id = ?2
                       AND ma.kind IN ('from','to','cc')
                       AND c.is_self = 0
                       AND c.is_hidden = 0
                     ON CONFLICT(src_type, src_id, dst_type, dst_id, relation)
                     DO UPDATE SET provenance = excluded.provenance,
                                   updated_at = excluded.updated_at
                     WHERE links.origin != 'manual'",
                    params![project_id, message_id, now],
                )?;
            }
        } else {
            tx.execute(
                "DELETE FROM links
                 WHERE src_type = 'project' AND src_id = ?1
                   AND dst_type = 'message' AND dst_id = ?2
                   AND relation = 'includes'",
                params![project_id, message_id],
            )?;
            // Note: auto-contacts added by a previous link are intentionally
            // kept when the message is unlinked (per spec).
        }

        // Timeline: only real changes (a repeated link/unlink adds nothing).
        // Reference ids only, never subject or body.
        if linked != was_linked {
            insert_entry(
                &tx,
                project_id,
                TimelineKind::Activity,
                if linked {
                    "Message linked to project"
                } else {
                    "Message unlinked from project"
                },
                Some((EntityKind::Message, message_id)),
                now,
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    /// Link or unlink a contact from a project with `origin='manual'`. Idempotent.
    /// Linking over an existing `'auto'` row upgrades it to `'manual'`.
    pub fn set_contact_link(
        &self,
        project_id: &str,
        contact_id: i64,
        linked: bool,
    ) -> Result<(), DbError> {
        let now = now_epoch();
        let conn = self.lock_conn();
        if linked {
            // Manual wins the origin; the original derived provenance and any
            // user note are preserved, but stale derived confidence is cleared
            // (a manual link carries no machine confidence — links invariants).
            conn.execute(
                "INSERT INTO links
                     (src_type, src_id, dst_type, dst_id, relation, origin,
                      provenance, confidence, note, created_at, updated_at)
                 VALUES ('project', ?1, 'contact', CAST(?2 AS TEXT), 'involves',
                         'manual', NULL, NULL, NULL, ?3, ?3)
                 ON CONFLICT(src_type, src_id, dst_type, dst_id, relation)
                 DO UPDATE SET origin = 'manual', confidence = NULL,
                               updated_at = excluded.updated_at",
                params![project_id, contact_id, now],
            )?;
        } else {
            conn.execute(
                "DELETE FROM links
                 WHERE src_type = 'project' AND src_id = ?1
                   AND dst_type = 'contact' AND dst_id = CAST(?2 AS TEXT)
                   AND relation = 'involves'",
                params![project_id, contact_id],
            )?;
        }
        Ok(())
    }

    /// Promote an `'auto'` contact link to `'manual'` (confirm auto-suggestion).
    /// A no-op when the contact is already manual or not linked at all.
    pub fn confirm_auto_contact(&self, project_id: &str, contact_id: i64) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "UPDATE links SET origin = 'manual', confidence = NULL, updated_at = ?3
             WHERE src_type = 'project' AND src_id = ?1
               AND dst_type = 'contact' AND dst_id = CAST(?2 AS TEXT)
               AND relation = 'involves' AND origin = 'derived'",
            params![project_id, contact_id, now_epoch()],
        )?;
        Ok(())
    }

    /// Projects that contain `message_id` (reverse lookup for the reading pane).
    pub fn projects_for_message(&self, message_id: &str) -> Result<Vec<ProjectTag>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT p.id, p.name, p.color
             FROM links pe
             JOIN projects p ON p.id = pe.src_id
             WHERE pe.dst_type = 'message' AND pe.dst_id = ?1
               AND pe.src_type = 'project' AND pe.relation = 'includes'
             ORDER BY p.name",
        )?;
        let rows = stmt.query_map(params![message_id], |r| {
            Ok(ProjectTag {
                project_id: r.get(0)?,
                name: r.get(1)?,
                color: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Move a project to a new status column and position in one atomic
    /// transaction. Re-packs positions in both the source column (gap removed)
    /// and the destination column (insert at `new_position`, shift others).
    pub fn move_project(
        &self,
        id: &str,
        new_status: &str,
        new_position: i64,
    ) -> Result<(), DbError> {
        let now = now_epoch();
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;

        // Read the old status so we can re-pack the source column after the move.
        let old_status: Option<String> = tx
            .query_row(
                "SELECT status FROM projects WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        let old_status = match old_status {
            Some(s) => s,
            None => return Err(DbError(format!("project {id} not found"))),
        };

        // Apply the move.
        tx.execute(
            "UPDATE projects SET status = ?2, position = ?3, updated_at = ?4
             WHERE id = ?1",
            params![id, new_status, new_position, now],
        )?;

        // Shift existing rows in the destination column to make room.
        tx.execute(
            "UPDATE projects SET position = position + 1
             WHERE status = ?1 AND id != ?2 AND position >= ?3",
            params![new_status, id, new_position],
        )?;

        if old_status != new_status {
            // Re-pack the source column (the moved project is no longer in it).
            let to_repack: Vec<String> = collect_ids(
                &tx,
                "SELECT id FROM projects WHERE status = ?1 AND id != ?2
                 ORDER BY position, id",
                rusqlite::params![old_status.as_str(), id],
            )?;
            for (i, pid) in to_repack.iter().enumerate() {
                tx.execute(
                    "UPDATE projects SET position = ?2 WHERE id = ?1",
                    params![pid, i as i64],
                )?;
            }
        } else {
            // Same column move: re-pack all rows in this column.
            let all: Vec<String> = collect_ids(
                &tx,
                "SELECT id FROM projects WHERE status = ?1 ORDER BY position, id",
                rusqlite::params![new_status],
            )?;
            for (i, pid) in all.iter().enumerate() {
                tx.execute(
                    "UPDATE projects SET position = ?2 WHERE id = ?1",
                    params![pid, i as i64],
                )?;
            }
        }

        tx.commit()?;
        Ok(())
    }
}

fn row_to_project(r: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: r.get(0)?,
        name: r.get(1)?,
        notes: r.get(2)?,
        status: r.get(3)?,
        color: r.get(4)?,
        position: r.get(5)?,
        archived: r.get::<_, i64>(6)? != 0,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
        last_activity_at: r.get(9)?,
    })
}

fn row_to_card(r: &rusqlite::Row<'_>) -> rusqlite::Result<ProjectCard> {
    Ok(ProjectCard {
        id: r.get(0)?,
        name: r.get(1)?,
        notes: r.get(2)?,
        status: r.get(3)?,
        color: r.get(4)?,
        position: r.get(5)?,
        archived: r.get::<_, i64>(6)? != 0,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
        last_activity_at: r.get(9)?,
        email_count: r.get(10)?,
        contact_count: r.get(11)?,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use castellum_models::{Account, Address, Mailbox, NewMessage};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn fresh_db() -> Database {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-projects-test-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        Database::open(&path).expect("open")
    }

    fn seed_account(db: &Database) {
        db.upsert_account(&Account {
            id: "acc1".into(),
            email: "me@local.test".into(),
            display_name: Some("Me".into()),
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
    }

    fn insert_msg(db: &Database, uid: i64, addresses: Vec<Address>) -> String {
        let from = addresses.iter().find(|a| a.kind == "from");
        let id = format!("acc1:INBOX:{uid}");
        db.upsert_message(&NewMessage {
            id: id.clone(),
            account_id: "acc1".into(),
            mailbox_id: "acc1:INBOX".into(),
            provider_uid: Some(uid),
            rfc822_message_id: Some(format!("<{uid}@local.test>")),
            subject: Some(format!("Subject {uid}")),
            from_name: from.and_then(|a| a.name.clone()),
            from_address: from.map(|a| a.address.clone()),
            sent_at: Some(1_800_000_000 + uid * 60),
            snippet: None,
            size_bytes: None,
            has_attachments: false,
            attachments: None,
            is_read: false,
            is_starred: false,
            addresses,
            text_body: None,
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: Vec::new(),
        })
        .unwrap();
        id
    }

    fn addr(kind: &str, address: &str) -> Address {
        Address {
            kind: kind.into(),
            name: None,
            address: address.into(),
        }
    }

    // Seed contacts so set_email_link's auto_contacts path can find them.
    fn seed_contacts(db: &Database) {
        db.refresh_contact_aggregates(1_800_000_000).unwrap();
    }

    #[test]
    fn upsert_and_list_projects_with_counts() {
        let db = fresh_db();
        let p1 = db
            .upsert_project(ProjectUpsert {
                name: "Alpha",
                status: "active",
                position: 0,
                ..Default::default()
            })
            .unwrap();
        assert!(!p1.id.is_empty());
        assert_eq!(p1.name, "Alpha");
        assert_eq!(p1.status, "active");

        let _p2 = db
            .upsert_project(ProjectUpsert {
                name: "Beta",
                notes: "notes",
                status: "waiting",
                color: Some("#ff0000"),
                position: 1,
                ..Default::default()
            })
            .unwrap();

        // Update p1 by id.
        let updated = db
            .upsert_project(ProjectUpsert {
                id: Some(&p1.id),
                name: "Alpha Updated",
                notes: "n",
                status: "active",
                position: 0,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(updated.name, "Alpha Updated");
        assert_eq!(updated.id, p1.id);

        let cards = db.list_projects_with_counts(false).unwrap();
        assert_eq!(cards.len(), 2);
        // First should be active (alpha), second waiting (beta).
        assert_eq!(cards[0].status, "active");
        assert_eq!(cards[1].status, "waiting");
        assert_eq!(cards[0].email_count, 0);
    }

    #[test]
    fn status_check_rejects_invalid() {
        let db = fresh_db();
        // SQLite CHECK constraint rejects an invalid status.
        let err = db
            .upsert_project(ProjectUpsert {
                name: "X",
                status: "invalid_status",
                position: 0,
                ..Default::default()
            })
            .unwrap_err();
        assert!(
            err.0.contains("CHECK"),
            "expected CHECK violation, got: {}",
            err.0
        );
    }

    #[test]
    fn archived_flag_set_and_excluded_from_default_list() {
        let db = fresh_db();
        let p = db
            .upsert_project(ProjectUpsert {
                name: "Old work",
                status: "active",
                position: 0,
                ..Default::default()
            })
            .unwrap();
        assert!(!p.archived, "new projects are not archived");

        // Archive via upsert with archived=Some(true).
        let archived = db
            .upsert_project(ProjectUpsert {
                id: Some(&p.id),
                name: "Old work",
                status: "active",
                position: 0,
                archived: Some(true),
                ..Default::default()
            })
            .unwrap();
        assert!(archived.archived, "archived flag must persist");

        // Excluded from the default (non-archived) list...
        let visible = db.list_projects_with_counts(false).unwrap();
        assert!(visible.is_empty(), "archived project must not be listed");
        // ...but present when include_archived=true.
        let all = db.list_projects_with_counts(true).unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].archived);

        // An upsert with archived=None must NOT clobber the stored flag.
        let untouched = db
            .upsert_project(ProjectUpsert {
                id: Some(&p.id),
                name: "Renamed",
                status: "active",
                position: 0,
                archived: None,
                ..Default::default()
            })
            .unwrap();
        assert!(untouched.archived, "archived=None must keep the flag");

        // Unarchive via archived=Some(false).
        let restored = db
            .upsert_project(ProjectUpsert {
                id: Some(&p.id),
                name: "Renamed",
                status: "active",
                position: 0,
                archived: Some(false),
                ..Default::default()
            })
            .unwrap();
        assert!(!restored.archived);
        assert_eq!(db.list_projects_with_counts(false).unwrap().len(), 1);
    }

    #[test]
    fn link_email_idempotent() {
        let db = fresh_db();
        seed_account(&db);
        let mid = insert_msg(
            &db,
            1,
            vec![addr("from", "alice@x.test"), addr("to", "me@local.test")],
        );
        let p = db
            .upsert_project(ProjectUpsert {
                name: "P",
                status: "active",
                position: 0,
                ..Default::default()
            })
            .unwrap();

        db.set_email_link(&p.id, &mid, true, false).unwrap();
        db.set_email_link(&p.id, &mid, true, false).unwrap(); // idempotent
        let cards = db.list_projects_with_counts(false).unwrap();
        assert_eq!(
            cards[0].email_count, 1,
            "idempotent link must not duplicate"
        );

        db.set_email_link(&p.id, &mid, false, false).unwrap();
        db.set_email_link(&p.id, &mid, false, false).unwrap(); // idempotent
        let cards = db.list_projects_with_counts(false).unwrap();
        assert_eq!(cards[0].email_count, 0);
    }

    #[test]
    fn link_email_auto_adds_participants_as_auto() {
        let db = fresh_db();
        seed_account(&db);
        // Insert a message with multiple non-self participants.
        let mid = insert_msg(
            &db,
            1,
            vec![
                addr("from", "alice@x.test"),
                addr("to", "me@local.test"),
                addr("cc", "bob@x.test"),
            ],
        );
        seed_contacts(&db); // creates contacts from message_addresses
        let p = db
            .upsert_project(ProjectUpsert {
                name: "P",
                status: "active",
                position: 0,
                ..Default::default()
            })
            .unwrap();
        db.set_email_link(&p.id, &mid, true, true).unwrap();

        let detail = db.get_project_hydrated(&p.id).unwrap().unwrap();
        // alice and bob should be auto-linked; me@local.test is is_self, excluded.
        let autos: Vec<_> = detail
            .contacts
            .iter()
            .filter(|c| c.origin == "auto")
            .collect();
        assert_eq!(autos.len(), 2, "alice + bob auto-linked");
        assert!(autos.iter().any(|c| c.email == "alice@x.test"));
        assert!(autos.iter().any(|c| c.email == "bob@x.test"));
        // me@local.test must NOT be linked.
        assert!(!detail.contacts.iter().any(|c| c.email == "me@local.test"));
    }

    #[test]
    fn manual_link_not_downgraded_by_auto() {
        let db = fresh_db();
        seed_account(&db);
        let mid = insert_msg(
            &db,
            1,
            vec![addr("from", "alice@x.test"), addr("to", "me@local.test")],
        );
        seed_contacts(&db);
        let p = db
            .upsert_project(ProjectUpsert {
                name: "P",
                status: "active",
                position: 0,
                ..Default::default()
            })
            .unwrap();

        // Manually link alice first.
        let alice_id = db.list_contacts(Some("alice"), true, 10).unwrap()[0].id;
        db.set_contact_link(&p.id, alice_id, true).unwrap();
        // Now auto-link the message (alice is a participant).
        db.set_email_link(&p.id, &mid, true, true).unwrap();

        let detail = db.get_project_hydrated(&p.id).unwrap().unwrap();
        let alice_link = detail
            .contacts
            .iter()
            .find(|c| c.email == "alice@x.test")
            .unwrap();
        assert_eq!(
            alice_link.origin, "manual",
            "manual must not be downgraded to auto"
        );
    }

    #[test]
    fn auto_link_upgraded_by_manual() {
        let db = fresh_db();
        seed_account(&db);
        let mid = insert_msg(
            &db,
            1,
            vec![addr("from", "alice@x.test"), addr("to", "me@local.test")],
        );
        seed_contacts(&db);
        let p = db
            .upsert_project(ProjectUpsert {
                name: "P",
                status: "active",
                position: 0,
                ..Default::default()
            })
            .unwrap();

        // Auto-link alice via message.
        db.set_email_link(&p.id, &mid, true, true).unwrap();
        let alice_id = db.list_contacts(Some("alice"), true, 10).unwrap()[0].id;
        let before = db.get_project_hydrated(&p.id).unwrap().unwrap();
        let before_link = before
            .contacts
            .iter()
            .find(|c| c.email == "alice@x.test")
            .unwrap();
        assert_eq!(before_link.origin, "auto");

        // Manually confirm.
        db.set_contact_link(&p.id, alice_id, true).unwrap();
        let after = db.get_project_hydrated(&p.id).unwrap().unwrap();
        let after_link = after
            .contacts
            .iter()
            .find(|c| c.email == "alice@x.test")
            .unwrap();
        assert_eq!(after_link.origin, "manual", "manual link must upgrade auto");
    }

    #[test]
    fn auto_link_refreshes_derived_evidence_but_never_manual_rows() {
        use crate::{EntityKind, LinkEnd};
        let db = fresh_db();
        seed_account(&db);
        let m1 = insert_msg(
            &db,
            1,
            vec![addr("from", "alice@x.test"), addr("to", "me@local.test")],
        );
        let m2 = insert_msg(
            &db,
            2,
            vec![addr("from", "alice@x.test"), addr("to", "me@local.test")],
        );
        seed_contacts(&db);
        let p = db
            .upsert_project(ProjectUpsert {
                name: "P",
                status: "active",
                position: 0,
                ..Default::default()
            })
            .unwrap();
        db.set_email_link(&p.id, &m1, true, true).unwrap();
        db.set_email_link(&p.id, &m2, true, true).unwrap();

        let alice_id = db.list_contacts(Some("alice"), true, 10).unwrap()[0].id;
        let alice_end = alice_id.to_string();
        let link = db
            .link_between(
                LinkEnd::new(EntityKind::Project, &p.id),
                LinkEnd::new(EntityKind::Contact, &alice_end),
                "involves",
            )
            .unwrap()
            .unwrap();
        assert_eq!(link.origin, "derived");
        assert_eq!(
            link.provenance.as_deref(),
            Some(format!("message:{m2}").as_str()),
            "derived evidence refreshes to the latest evidencing message"
        );

        // Promote to manual, then auto-link again: the manual row is frozen.
        db.set_contact_link(&p.id, alice_id, true).unwrap();
        db.set_email_link(&p.id, &m1, true, true).unwrap();
        let link = db
            .link_between(
                LinkEnd::new(EntityKind::Project, &p.id),
                LinkEnd::new(EntityKind::Contact, &alice_end),
                "involves",
            )
            .unwrap()
            .unwrap();
        assert_eq!(link.origin, "manual", "auto pass never downgrades manual");
        assert_eq!(
            link.provenance.as_deref(),
            Some(format!("message:{m2}").as_str()),
            "auto pass never rewrites a manual row's evidence"
        );
    }

    #[test]
    fn promoting_a_confident_derived_contact_link_clears_confidence() {
        // Both raw-SQL promotion paths (set_contact_link ON CONFLICT and
        // confirm_auto_contact) must drop stale derived confidence, exactly
        // like upsert_link's manual arm — a manual link carries none.
        use crate::{EntityKind, LinkEnd, LinkUpsert};
        let db = fresh_db();
        seed_account(&db);
        insert_msg(
            &db,
            1,
            vec![addr("from", "alice@x.test"), addr("to", "me@local.test")],
        );
        seed_contacts(&db);
        let p = db
            .upsert_project(ProjectUpsert {
                name: "P",
                status: "active",
                position: 0,
                ..Default::default()
            })
            .unwrap();
        let alice_id = db.list_contacts(Some("alice"), true, 10).unwrap()[0].id;
        let alice_end = alice_id.to_string();
        let seed_confident_derived = || {
            db.upsert_link(LinkUpsert {
                src: LinkEnd::new(EntityKind::Project, &p.id),
                dst: LinkEnd::new(EntityKind::Contact, &alice_end),
                relation: "involves",
                origin: "derived",
                provenance: Some("message:acc1:INBOX:1"),
                confidence: Some(0.6),
            })
            .unwrap();
        };
        let read = || {
            db.link_between(
                LinkEnd::new(EntityKind::Project, &p.id),
                LinkEnd::new(EntityKind::Contact, &alice_end),
                "involves",
            )
            .unwrap()
            .unwrap()
        };

        // Path 1: set_contact_link.
        seed_confident_derived();
        db.set_contact_link(&p.id, alice_id, true).unwrap();
        let link = read();
        assert_eq!(link.origin, "manual");
        assert_eq!(link.confidence, None, "set_contact_link clears confidence");
        assert_eq!(
            link.provenance.as_deref(),
            Some("message:acc1:INBOX:1"),
            "evidence preserved"
        );

        // Path 2: confirm_auto_contact (reset to derived first).
        db.unlink(
            LinkEnd::new(EntityKind::Project, &p.id),
            LinkEnd::new(EntityKind::Contact, &alice_end),
            "involves",
        )
        .unwrap();
        seed_confident_derived();
        db.confirm_auto_contact(&p.id, alice_id).unwrap();
        let link = read();
        assert_eq!(link.origin, "manual");
        assert_eq!(
            link.confidence, None,
            "confirm_auto_contact clears confidence"
        );
    }

    #[test]
    fn unlink_email_keeps_auto_contacts() {
        let db = fresh_db();
        seed_account(&db);
        let mid = insert_msg(
            &db,
            1,
            vec![addr("from", "alice@x.test"), addr("to", "me@local.test")],
        );
        seed_contacts(&db);
        let p = db
            .upsert_project(ProjectUpsert {
                name: "P",
                status: "active",
                position: 0,
                ..Default::default()
            })
            .unwrap();
        db.set_email_link(&p.id, &mid, true, true).unwrap();
        // Unlink the message.
        db.set_email_link(&p.id, &mid, false, false).unwrap();

        let detail = db.get_project_hydrated(&p.id).unwrap().unwrap();
        assert_eq!(detail.emails.len(), 0, "email unlinked");
        // Auto contacts survive the message unlink.
        assert!(
            !detail.contacts.is_empty(),
            "auto contacts kept after unlink"
        );
    }

    #[test]
    fn delete_project_cascades_link_rows() {
        let db = fresh_db();
        seed_account(&db);
        let mid = insert_msg(
            &db,
            1,
            vec![addr("from", "alice@x.test"), addr("to", "me@local.test")],
        );
        seed_contacts(&db);
        let p = db
            .upsert_project(ProjectUpsert {
                name: "P",
                status: "active",
                position: 0,
                ..Default::default()
            })
            .unwrap();
        db.set_email_link(&p.id, &mid, true, true).unwrap();

        db.delete_project(&p.id).unwrap();
        assert_eq!(db.list_projects_with_counts(false).unwrap().len(), 0);
        // projects_for_message should return nothing now.
        assert!(db.projects_for_message(&mid).unwrap().is_empty());
    }

    #[test]
    fn deleting_message_cascades_project_email_link() {
        let db = fresh_db();
        seed_account(&db);
        let mid = insert_msg(
            &db,
            1,
            vec![addr("from", "alice@x.test"), addr("to", "me@local.test")],
        );
        let p = db
            .upsert_project(ProjectUpsert {
                name: "P",
                status: "active",
                position: 0,
                ..Default::default()
            })
            .unwrap();
        db.set_email_link(&p.id, &mid, true, false).unwrap();

        // Simulate sync deleting the message row.
        {
            let conn = db.lock_conn();
            conn.execute("DELETE FROM messages WHERE id = ?1", params![mid])
                .unwrap();
        }
        let cards = db.list_projects_with_counts(false).unwrap();
        assert_eq!(
            cards[0].email_count, 0,
            "FK cascade removed project_email row"
        );
    }

    #[test]
    fn projects_for_message_reverse_lookup() {
        let db = fresh_db();
        seed_account(&db);
        let mid = insert_msg(
            &db,
            1,
            vec![addr("from", "alice@x.test"), addr("to", "me@local.test")],
        );
        let p1 = db
            .upsert_project(ProjectUpsert {
                name: "P1",
                status: "active",
                position: 0,
                ..Default::default()
            })
            .unwrap();
        let p2 = db
            .upsert_project(ProjectUpsert {
                name: "P2",
                status: "waiting",
                position: 0,
                ..Default::default()
            })
            .unwrap();
        db.set_email_link(&p1.id, &mid, true, false).unwrap();
        db.set_email_link(&p2.id, &mid, true, false).unwrap();

        let tags = db.projects_for_message(&mid).unwrap();
        assert_eq!(tags.len(), 2);
        let names: Vec<_> = tags.iter().map(|t| &t.name).collect();
        assert!(names.contains(&&"P1".to_string()));
        assert!(names.contains(&&"P2".to_string()));
    }

    #[test]
    fn move_project_repacks_positions() {
        let db = fresh_db();
        // Three active projects at positions 0, 1, 2.
        let p0 = db
            .upsert_project(ProjectUpsert {
                name: "A",
                status: "active",
                position: 0,
                ..Default::default()
            })
            .unwrap();
        let _p1 = db
            .upsert_project(ProjectUpsert {
                name: "B",
                status: "active",
                position: 1,
                ..Default::default()
            })
            .unwrap();
        let _p2 = db
            .upsert_project(ProjectUpsert {
                name: "C",
                status: "active",
                position: 2,
                ..Default::default()
            })
            .unwrap();

        // Move p0 to "waiting" at position 0.
        db.move_project(&p0.id, "waiting", 0).unwrap();

        // Active column should now have B(0) and C(1).
        let active: Vec<_> = db
            .list_projects_with_counts(false)
            .unwrap()
            .into_iter()
            .filter(|c| c.status == "active")
            .collect();
        assert_eq!(active.len(), 2);
        let positions: Vec<i64> = active.iter().map(|c| c.position).collect();
        assert!(positions.contains(&0) && positions.contains(&1));

        // Waiting column should have p0 at 0.
        let waiting: Vec<_> = db
            .list_projects_with_counts(false)
            .unwrap()
            .into_iter()
            .filter(|c| c.status == "waiting")
            .collect();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].id, p0.id);
        assert_eq!(waiting[0].position, 0);
    }

    #[test]
    fn hydrate_returns_headers_and_contacts() {
        let db = fresh_db();
        seed_account(&db);
        let mid = insert_msg(
            &db,
            1,
            vec![addr("from", "alice@x.test"), addr("to", "me@local.test")],
        );
        seed_contacts(&db);
        let p = db
            .upsert_project(ProjectUpsert {
                name: "P",
                status: "active",
                color: Some("#blue"),
                position: 0,
                ..Default::default()
            })
            .unwrap();
        db.set_email_link(&p.id, &mid, true, true).unwrap();

        let detail = db.get_project_hydrated(&p.id).unwrap().unwrap();
        assert_eq!(detail.project.name, "P");
        assert_eq!(detail.emails.len(), 1);
        assert_eq!(detail.emails[0].id, mid);
        // alice should appear as auto contact.
        assert!(!detail.contacts.is_empty());
    }

    #[test]
    fn archived_excluded_by_default() {
        let db = fresh_db();
        let p = db
            .upsert_project(ProjectUpsert {
                name: "P",
                status: "active",
                position: 0,
                ..Default::default()
            })
            .unwrap();
        // Archive by direct SQL (the archive flag is set via upsert).
        {
            let conn = db.lock_conn();
            conn.execute(
                "UPDATE projects SET archived = 1 WHERE id = ?1",
                params![p.id],
            )
            .unwrap();
        }
        assert!(db.list_projects_with_counts(false).unwrap().is_empty());
        assert_eq!(db.list_projects_with_counts(true).unwrap().len(), 1);
    }
}
