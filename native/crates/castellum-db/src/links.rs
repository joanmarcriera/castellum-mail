// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//! Shared evidence-backed link model (migration 0017).
//!
//! One generic `links` table is the single source of truth for cross-entity
//! relationships. Invariants enforced here and pinned by tests:
//!
//! - **Idempotent**: re-upserting the same (src, dst, relation) never
//!   duplicates; a repeated manual link just bumps `updated_at`.
//! - **Manual is never downgraded**: a derived/source upsert over a manual
//!   row is a no-op; a manual upsert over a derived row upgrades the origin
//!   while PRESERVING provenance (the original evidence) and the user note.
//! - **Derived refreshes** update provenance/confidence/updated_at on
//!   derived rows only, and never touch `note`.
//! - **User notes are user-owned**: only `set_link_note` writes them.
//! - **No copied content**: provenance is a short reference (entity ids /
//!   source names), enforced by length CHECKs — never message text.
//! - **Confidence** is only meaningful for non-manual links and lives in
//!   [0, 1] (schema CHECK).

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::{Database, DbError};

/// Entity kinds a link endpoint can reference. `as_str` values are persisted;
/// never rename.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntityKind {
    Project,
    Message,
    Thread,
    Contact,
    WorkItem,
}

impl EntityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EntityKind::Project => "project",
            EntityKind::Message => "message",
            EntityKind::Thread => "thread",
            EntityKind::Contact => "contact",
            EntityKind::WorkItem => "work_item",
        }
    }

    /// Inverse of `as_str`; `None` for unknown kinds (callers must reject).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "project" => Some(EntityKind::Project),
            "message" => Some(EntityKind::Message),
            "thread" => Some(EntityKind::Thread),
            "contact" => Some(EntityKind::Contact),
            "work_item" => Some(EntityKind::WorkItem),
            _ => None,
        }
    }
}

/// One typed link endpoint.
#[derive(Clone, Copy, Debug)]
pub struct LinkEnd<'a> {
    pub kind: EntityKind,
    pub id: &'a str,
}

impl<'a> LinkEnd<'a> {
    pub fn new(kind: EntityKind, id: &'a str) -> Self {
        LinkEnd { kind, id }
    }
}

/// Input for an idempotent link upsert.
#[derive(Clone, Copy, Debug)]
pub struct LinkUpsert<'a> {
    pub src: LinkEnd<'a>,
    pub dst: LinkEnd<'a>,
    pub relation: &'a str,
    /// 'manual', 'derived', or a named source (e.g. 'linkedin-import').
    pub origin: &'a str,
    /// Evidence reference answering "why is this connected?" — entity ids or
    /// source names only, never copied content.
    pub provenance: Option<&'a str>,
    /// Only valid for non-manual origins.
    pub confidence: Option<f64>,
}

/// A stored link row. Serialized snake_case for the FFI/Swift boundary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinkRecord {
    pub id: i64,
    pub src_kind: String,
    pub src_id: String,
    pub dst_kind: String,
    pub dst_id: String,
    pub relation: String,
    pub origin: String,
    pub provenance: Option<String>,
    pub confidence: Option<f64>,
    pub note: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn row_to_link(r: &rusqlite::Row<'_>) -> rusqlite::Result<LinkRecord> {
    Ok(LinkRecord {
        id: r.get(0)?,
        src_kind: r.get(1)?,
        src_id: r.get(2)?,
        dst_kind: r.get(3)?,
        dst_id: r.get(4)?,
        relation: r.get(5)?,
        origin: r.get(6)?,
        provenance: r.get(7)?,
        confidence: r.get(8)?,
        note: r.get(9)?,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
    })
}

const LINK_COLUMNS: &str = "id, src_type, src_id, dst_type, dst_id, relation,
                            origin, provenance, confidence, note, created_at, updated_at";

fn validate_relation(relation: &str) -> Result<(), DbError> {
    let ok = !relation.is_empty()
        && relation.len() <= 64
        && relation
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(DbError(format!(
            "invalid relation {relation:?}: lowercase ascii/digits/underscore, 1-64 chars"
        )))
    }
}

fn validate_origin(origin: &str) -> Result<(), DbError> {
    let ok = !origin.is_empty()
        && origin.len() <= 64
        && origin
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(DbError(format!(
            "invalid origin {origin:?}: lowercase ascii/digits/underscore/hyphen, 1-64 chars"
        )))
    }
}

/// Provenance is an evidence REFERENCE — never copied content. The schema
/// only bounds its length; this enforces the shape structurally so a caller
/// cannot smuggle a message body or free prose into a relationship row.
///
/// Accepted grammar (all ≤512 bytes, no whitespace, no control chars):
///   * the legacy sentinel `participant` (pre-0017 auto-contact links carried
///     no evidencing message id); or
///   * `<source>:<ref>` where `<source>` is a lowercase ascii/digit/`_`/`-`
///     token (e.g. `message`, `import`, `linkedin`) and `<ref>` is an
///     id-shaped reference that MAY itself contain `:` (so
///     `message:acc:INBOX:9` round-trips) plus a small id-safe punctuation set.
fn validate_provenance(provenance: Option<&str>) -> Result<(), DbError> {
    let Some(p) = provenance else {
        return Ok(());
    };
    if p.is_empty() || p.len() > 512 {
        return Err(DbError(
            "provenance must be a 1-512 char id reference".into(),
        ));
    }
    if p.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(DbError(format!(
            "invalid provenance {p:?}: ids only — no whitespace or control chars"
        )));
    }
    if p == "participant" {
        return Ok(());
    }
    let Some((source, reference)) = p.split_once(':') else {
        return Err(DbError(format!(
            "invalid provenance {p:?}: expected 'participant' or '<source>:<id>'"
        )));
    };
    let source_ok = !source.is_empty()
        && source
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    let reference_ok = !reference.is_empty()
        && reference.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '_' | '@' | '=' | '+' | '/' | '-')
        });
    if source_ok && reference_ok {
        Ok(())
    } else {
        Err(DbError(format!(
            "invalid provenance {p:?}: '<source>:<id>' with an id-shaped reference"
        )))
    }
}

/// Whether a concrete link endpoint resolves to an existing owned row.
///
/// `thread` is deliberately virtual: messages carry a `thread_id` but there is
/// no threads-owned table/lifecycle, so a thread endpoint is always accepted
/// and its link persists until explicitly unlinked. Every other kind must
/// resolve to a live row so a generic write cannot create a dangling link from
/// a typo or hostile FFI input. (Cascade triggers clean links up on delete;
/// this closes the write side.)
fn endpoint_exists(conn: &rusqlite::Connection, end: LinkEnd<'_>) -> Result<bool, rusqlite::Error> {
    let sql = match end.kind {
        EntityKind::Thread => return Ok(true),
        EntityKind::Project => "SELECT 1 FROM projects WHERE id = ?1",
        EntityKind::Message => "SELECT 1 FROM messages WHERE id = ?1",
        // contacts.id is INTEGER; links store it as text.
        EntityKind::Contact => "SELECT 1 FROM contacts WHERE CAST(id AS TEXT) = ?1",
        EntityKind::WorkItem => "SELECT 1 FROM work_items WHERE id = ?1",
    };
    conn.query_row(sql, params![end.id], |_| Ok(()))
        .optional()
        .map(|o| o.is_some())
}

impl Database {
    /// Idempotently create or refresh a link. Conflict rules (pinned by
    /// tests): a manual upsert always wins the origin and bumps `updated_at`
    /// while preserving provenance and the user note; a non-manual upsert
    /// refreshes provenance/confidence/updated_at on non-manual rows and is
    /// a complete no-op on manual rows. Returns the resulting row.
    pub fn upsert_link(&self, l: LinkUpsert<'_>) -> Result<LinkRecord, DbError> {
        let conn = self.lock_conn();
        upsert_link_in(&conn, l)
    }
}

/// [`Database::upsert_link`] on an already-open connection or transaction,
/// so callers can make the link write atomic with related writes.
pub(crate) fn upsert_link_in(
    conn: &rusqlite::Connection,
    l: LinkUpsert<'_>,
) -> Result<LinkRecord, DbError> {
    {
        validate_relation(l.relation)?;
        validate_origin(l.origin)?;
        validate_provenance(l.provenance)?;
        if l.origin == "manual" && l.confidence.is_some() {
            return Err(DbError(
                "confidence is only valid for derived/source links".into(),
            ));
        }
        if let Some(c) = l.confidence {
            if !(0.0..=1.0).contains(&c) {
                return Err(DbError(format!("confidence {c} outside [0, 1]")));
            }
        }
        let now = now_epoch();
        // Reject dangling endpoints structurally (thread is virtual — always ok).
        for (label, end) in [("src", l.src), ("dst", l.dst)] {
            if !endpoint_exists(conn, end)? {
                return Err(DbError(format!(
                    "{label} endpoint {}:{} does not exist",
                    end.kind.as_str(),
                    end.id
                )));
            }
        }
        if l.origin == "manual" {
            // Promotion to manual PRESERVES the original evidence (provenance)
            // and the user note, but CLEARS confidence — confidence is
            // meaningless once a human has affirmed the link.
            conn.execute(
                "INSERT INTO links (src_type, src_id, dst_type, dst_id, relation,
                                    origin, provenance, confidence, note,
                                    created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'manual', ?6, NULL, NULL, ?7, ?7)
                 ON CONFLICT(src_type, src_id, dst_type, dst_id, relation)
                 DO UPDATE SET origin = 'manual', confidence = NULL,
                               updated_at = excluded.updated_at",
                params![
                    l.src.kind.as_str(),
                    l.src.id,
                    l.dst.kind.as_str(),
                    l.dst.id,
                    l.relation,
                    l.provenance,
                    now
                ],
            )?;
        } else {
            conn.execute(
                "INSERT INTO links (src_type, src_id, dst_type, dst_id, relation,
                                    origin, provenance, confidence, note,
                                    created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, ?9)
                 ON CONFLICT(src_type, src_id, dst_type, dst_id, relation)
                 DO UPDATE SET provenance = excluded.provenance,
                               confidence = excluded.confidence,
                               origin = excluded.origin,
                               updated_at = excluded.updated_at
                 WHERE links.origin != 'manual'",
                params![
                    l.src.kind.as_str(),
                    l.src.id,
                    l.dst.kind.as_str(),
                    l.dst.id,
                    l.relation,
                    l.origin,
                    l.provenance,
                    l.confidence,
                    now
                ],
            )?;
        }
        let row = conn
            .query_row(
                &format!(
                    "SELECT {LINK_COLUMNS} FROM links
                     WHERE src_type = ?1 AND src_id = ?2
                       AND dst_type = ?3 AND dst_id = ?4 AND relation = ?5"
                ),
                params![
                    l.src.kind.as_str(),
                    l.src.id,
                    l.dst.kind.as_str(),
                    l.dst.id,
                    l.relation
                ],
                row_to_link,
            )
            .map_err(|e| DbError(format!("link row missing after upsert: {e}")))?;
        Ok(row)
    }
}

impl Database {
    /// Remove a link. Idempotent; returns whether a row existed.
    pub fn unlink(
        &self,
        src: LinkEnd<'_>,
        dst: LinkEnd<'_>,
        relation: &str,
    ) -> Result<bool, DbError> {
        let conn = self.lock_conn();
        let n = conn.execute(
            "DELETE FROM links
             WHERE src_type = ?1 AND src_id = ?2
               AND dst_type = ?3 AND dst_id = ?4 AND relation = ?5",
            params![
                src.kind.as_str(),
                src.id,
                dst.kind.as_str(),
                dst.id,
                relation
            ],
        )?;
        Ok(n > 0)
    }

    /// Set or clear the user-owned note on an existing link. Errors when the
    /// link does not exist (notes never create links).
    pub fn set_link_note(
        &self,
        src: LinkEnd<'_>,
        dst: LinkEnd<'_>,
        relation: &str,
        note: Option<&str>,
    ) -> Result<(), DbError> {
        let now = now_epoch();
        let conn = self.lock_conn();
        let n = conn.execute(
            "UPDATE links SET note = ?6, updated_at = ?7
             WHERE src_type = ?1 AND src_id = ?2
               AND dst_type = ?3 AND dst_id = ?4 AND relation = ?5",
            params![
                src.kind.as_str(),
                src.id,
                dst.kind.as_str(),
                dst.id,
                relation,
                note,
                now
            ],
        )?;
        if n == 0 {
            return Err(DbError("link not found for note".into()));
        }
        Ok(())
    }

    /// Every link touching the given endpoint (either direction), newest
    /// activity first — the "why is this connected?" read path.
    pub fn links_for(&self, end: LinkEnd<'_>) -> Result<Vec<LinkRecord>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {LINK_COLUMNS} FROM links
             WHERE (src_type = ?1 AND src_id = ?2)
                OR (dst_type = ?1 AND dst_id = ?2)
             ORDER BY updated_at DESC, id"
        ))?;
        let rows = stmt.query_map(params![end.kind.as_str(), end.id], row_to_link)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// A specific link, if present.
    pub fn link_between(
        &self,
        src: LinkEnd<'_>,
        dst: LinkEnd<'_>,
        relation: &str,
    ) -> Result<Option<LinkRecord>, DbError> {
        let conn = self.lock_conn();
        link_between_in(&conn, src, dst, relation)
    }
}

/// [`Database::link_between`] on an already-open connection or transaction,
/// so a caller can test for a link atomically with the write that follows.
pub(crate) fn link_between_in(
    conn: &rusqlite::Connection,
    src: LinkEnd<'_>,
    dst: LinkEnd<'_>,
    relation: &str,
) -> Result<Option<LinkRecord>, DbError> {
    let row = conn
        .query_row(
            &format!(
                "SELECT {LINK_COLUMNS} FROM links
                 WHERE src_type = ?1 AND src_id = ?2
                   AND dst_type = ?3 AND dst_id = ?4 AND relation = ?5"
            ),
            params![
                src.kind.as_str(),
                src.id,
                dst.kind.as_str(),
                dst.id,
                relation
            ],
            row_to_link,
        )
        .optional()?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use castellum_models::{Account, Mailbox, NewMessage};
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn fresh_db() -> Database {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-links-test-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        let db = Database::open(&path).expect("open");
        seed_endpoints(&db);
        db
    }

    /// Endpoint existence is now enforced on write, so tests link only real
    /// rows. Seed the fixed endpoint ids the tests reference: project `p1`,
    /// message `m1`, contacts `7`/`42`, work item `w1`.
    fn seed_endpoints(db: &Database) {
        db.upsert_account(&Account {
            id: "acc".into(),
            email: "me@local.test".into(),
            display_name: Some("Me".into()),
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
            id: "m1".into(),
            account_id: "acc".into(),
            mailbox_id: "acc:INBOX".into(),
            provider_uid: Some(1),
            rfc822_message_id: Some("<1@local.test>".into()),
            subject: Some("Subject".into()),
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
        let conn = db.lock_conn();
        conn.execute_batch(
            "INSERT INTO projects (id, name, status, position, created_at, updated_at, last_activity_at)
                 VALUES ('p1', 'P', 'active', 0, 0, 0, 0);
             INSERT INTO contacts (id, email, derived_name, is_self, is_hidden)
                 VALUES (7, 'c7@y.test', 'C7', 0, 0), (42, 'c42@y.test', 'C42', 0, 0);
             INSERT INTO work_items (id, project_id, title, state, position, created_at, updated_at)
                 VALUES ('w1', 'p1', 'W', 'waiting', 0, 0, 0);",
        )
        .unwrap();
    }

    fn project(id: &str) -> LinkEnd<'_> {
        LinkEnd::new(EntityKind::Project, id)
    }

    fn message(id: &str) -> LinkEnd<'_> {
        LinkEnd::new(EntityKind::Message, id)
    }

    fn manual<'a>(src: LinkEnd<'a>, dst: LinkEnd<'a>, relation: &'a str) -> LinkUpsert<'a> {
        LinkUpsert {
            src,
            dst,
            relation,
            origin: "manual",
            provenance: None,
            confidence: None,
        }
    }

    fn derived<'a>(
        src: LinkEnd<'a>,
        dst: LinkEnd<'a>,
        relation: &'a str,
        provenance: &'a str,
        confidence: f64,
    ) -> LinkUpsert<'a> {
        LinkUpsert {
            src,
            dst,
            relation,
            origin: "derived",
            provenance: Some(provenance),
            confidence: Some(confidence),
        }
    }

    #[test]
    fn manual_upsert_is_idempotent() {
        let db = fresh_db();
        let first = db
            .upsert_link(manual(project("p1"), message("m1"), "includes"))
            .unwrap();
        let second = db
            .upsert_link(manual(project("p1"), message("m1"), "includes"))
            .unwrap();
        assert_eq!(first.id, second.id, "no duplicate row");
        assert_eq!(second.origin, "manual");
        assert_eq!(first.created_at, second.created_at, "created_at is stable");
        let all = db.links_for(project("p1")).unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn derived_never_downgrades_manual() {
        let db = fresh_db();
        db.upsert_link(manual(project("p1"), message("m1"), "includes"))
            .unwrap();
        let after = db
            .upsert_link(derived(
                project("p1"),
                message("m1"),
                "includes",
                "message:m9",
                0.4,
            ))
            .unwrap();
        assert_eq!(after.origin, "manual", "derived upsert must not downgrade");
        assert_eq!(after.provenance, None, "manual row metadata untouched");
        assert_eq!(after.confidence, None);
    }

    #[test]
    fn manual_upgrades_derived_and_preserves_evidence_and_note() {
        let db = fresh_db();
        db.upsert_link(derived(
            project("p1"),
            message("m1"),
            "includes",
            "message:m9",
            0.7,
        ))
        .unwrap();
        db.set_link_note(project("p1"), message("m1"), "includes", Some("keep me"))
            .unwrap();
        let after = db
            .upsert_link(manual(project("p1"), message("m1"), "includes"))
            .unwrap();
        assert_eq!(after.origin, "manual", "manual upgrades derived");
        assert_eq!(
            after.provenance.as_deref(),
            Some("message:m9"),
            "the original evidence survives the upgrade"
        );
        assert_eq!(after.note.as_deref(), Some("keep me"), "user note survives");
    }

    #[test]
    fn derived_refresh_updates_evidence_but_never_the_note() {
        let db = fresh_db();
        db.upsert_link(derived(
            project("p1"),
            message("m1"),
            "includes",
            "message:m1",
            0.5,
        ))
        .unwrap();
        db.set_link_note(project("p1"), message("m1"), "includes", Some("mine"))
            .unwrap();
        let refreshed = db
            .upsert_link(derived(
                project("p1"),
                message("m1"),
                "includes",
                "message:m2",
                0.9,
            ))
            .unwrap();
        assert_eq!(refreshed.origin, "derived");
        assert_eq!(refreshed.provenance.as_deref(), Some("message:m2"));
        assert_eq!(refreshed.confidence, Some(0.9));
        assert_eq!(
            refreshed.note.as_deref(),
            Some("mine"),
            "derived refresh must never overwrite the user note"
        );
    }

    #[test]
    fn named_source_origin_round_trips() {
        let db = fresh_db();
        let row = db
            .upsert_link(LinkUpsert {
                src: project("p1"),
                dst: LinkEnd::new(EntityKind::Contact, "42"),
                relation: "involves",
                origin: "linkedin-import",
                provenance: Some("import:2026-07-10"),
                confidence: Some(1.0),
            })
            .unwrap();
        assert_eq!(row.origin, "linkedin-import");
        assert_eq!(row.provenance.as_deref(), Some("import:2026-07-10"));
    }

    #[test]
    fn confidence_rejected_for_manual_and_out_of_range() {
        let db = fresh_db();
        let mut bad = manual(project("p1"), message("m1"), "includes");
        bad.confidence = Some(0.5);
        assert!(
            db.upsert_link(bad).is_err(),
            "manual links carry no confidence"
        );

        let out_of_range = LinkUpsert {
            confidence: Some(1.5),
            ..derived(project("p1"), message("m1"), "includes", "message:m1", 0.5)
        };
        assert!(db.upsert_link(out_of_range).is_err());
    }

    #[test]
    fn relation_and_origin_are_validated() {
        let db = fresh_db();
        let mut l = manual(project("p1"), message("m1"), "includes");
        l.relation = "Bad Relation!";
        assert!(db.upsert_link(l).is_err());

        let mut l = manual(project("p1"), message("m1"), "includes");
        l.origin = "Not Valid Origin";
        assert!(db.upsert_link(l).is_err());
    }

    #[test]
    fn unlink_is_idempotent_and_reports_existence() {
        let db = fresh_db();
        db.upsert_link(manual(project("p1"), message("m1"), "includes"))
            .unwrap();
        assert!(db.unlink(project("p1"), message("m1"), "includes").unwrap());
        assert!(!db.unlink(project("p1"), message("m1"), "includes").unwrap());
        assert!(db.links_for(project("p1")).unwrap().is_empty());
    }

    #[test]
    fn note_requires_an_existing_link() {
        let db = fresh_db();
        assert!(db
            .set_link_note(project("p1"), message("m1"), "includes", Some("x"))
            .is_err());
    }

    #[test]
    fn links_for_sees_both_directions() {
        let db = fresh_db();
        db.upsert_link(manual(project("p1"), message("m1"), "includes"))
            .unwrap();
        db.upsert_link(manual(
            LinkEnd::new(EntityKind::WorkItem, "w1"),
            project("p1"),
            "belongs_to",
        ))
        .unwrap();
        let links = db.links_for(project("p1")).unwrap();
        assert_eq!(links.len(), 2, "src and dst matches both count");
        let between = db
            .link_between(project("p1"), message("m1"), "includes")
            .unwrap();
        assert!(between.is_some());
    }

    #[test]
    fn same_endpoints_different_relations_coexist() {
        let db = fresh_db();
        db.upsert_link(manual(project("p1"), message("m1"), "includes"))
            .unwrap();
        db.upsert_link(derived(
            project("p1"),
            message("m1"),
            "mentioned_in",
            "message:m1",
            0.3,
        ))
        .unwrap();
        assert_eq!(db.links_for(project("p1")).unwrap().len(), 2);
    }

    #[test]
    fn thread_endpoints_link_and_persist_until_unlinked() {
        // Threads are virtual (no owning table/lifecycle): a thread link must
        // round-trip like any other endpoint and persist until explicitly
        // unlinked — there is deliberately no cascade for threads.
        let db = fresh_db();
        let thread = LinkEnd::new(EntityKind::Thread, "t-abc");
        db.upsert_link(manual(project("p1"), thread, "includes"))
            .unwrap();
        let links = db.links_for(thread).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].dst_kind, "thread");
        assert!(db.unlink(project("p1"), thread, "includes").unwrap());
        assert!(db.links_for(thread).unwrap().is_empty());
    }

    #[test]
    fn contact_deletion_cascades_link_rows() {
        // The contacts pruning path (refresh_contact_aggregates) deletes
        // contact rows directly — the AFTER DELETE trigger must remove their
        // link rows in both endpoint positions. Contact 7 is seeded by
        // fresh_db().
        let db = fresh_db();
        let contact = LinkEnd::new(EntityKind::Contact, "7");
        db.upsert_link(manual(project("p1"), contact, "involves"))
            .unwrap();
        db.upsert_link(manual(contact, message("m1"), "mentioned_in"))
            .unwrap();
        {
            let conn = db.lock_conn();
            conn.execute("DELETE FROM contacts WHERE id = 7", [])
                .unwrap();
        }
        assert!(
            db.links_for(contact).unwrap().is_empty(),
            "the delete trigger removes links in both endpoint positions"
        );
        // Unrelated links survive.
        db.upsert_link(manual(project("p1"), message("m1"), "includes"))
            .unwrap();
        assert_eq!(db.links_for(project("p1")).unwrap().len(), 1);
    }

    #[test]
    fn manual_promotion_clears_stale_confidence() {
        // A derived link carries confidence; promoting it to manual must drop
        // it — confidence is meaningless once a human affirms the link — while
        // preserving the evidence and note.
        let db = fresh_db();
        db.upsert_link(derived(
            project("p1"),
            message("m1"),
            "includes",
            "message:m9",
            0.8,
        ))
        .unwrap();
        db.set_link_note(project("p1"), message("m1"), "includes", Some("mine"))
            .unwrap();
        let promoted = db
            .upsert_link(manual(project("p1"), message("m1"), "includes"))
            .unwrap();
        assert_eq!(promoted.origin, "manual");
        assert_eq!(
            promoted.confidence, None,
            "stale derived confidence is cleared on promotion"
        );
        assert_eq!(
            promoted.provenance.as_deref(),
            Some("message:m9"),
            "evidence survives"
        );
        assert_eq!(promoted.note.as_deref(), Some("mine"), "note survives");
    }

    #[test]
    fn upsert_rejects_nonexistent_endpoints() {
        let db = fresh_db();
        // Nonexistent project / contact / work_item / message are rejected.
        assert!(db
            .upsert_link(manual(project("ghost"), message("m1"), "includes"))
            .is_err());
        assert!(db
            .upsert_link(manual(
                project("p1"),
                LinkEnd::new(EntityKind::Contact, "999"),
                "involves",
            ))
            .is_err());
        assert!(db
            .upsert_link(manual(
                LinkEnd::new(EntityKind::WorkItem, "nope"),
                project("p1"),
                "belongs_to",
            ))
            .is_err());
        assert!(db
            .upsert_link(manual(project("p1"), message("m-ghost"), "includes"))
            .is_err());
        // No dangling rows were written.
        assert!(db.links_for(project("p1")).unwrap().is_empty());
    }

    #[test]
    fn thread_endpoints_skip_existence_check() {
        // Threads are virtual: an arbitrary thread id is always linkable.
        let db = fresh_db();
        let thread = LinkEnd::new(EntityKind::Thread, "thread-with-no-row");
        assert!(db
            .upsert_link(manual(project("p1"), thread, "includes"))
            .is_ok());
    }

    #[test]
    fn provenance_must_be_an_id_reference_not_free_text() {
        let db = fresh_db();
        // Legit id-shaped references (incl. colon-bearing message ids) pass.
        for prov in [
            "participant",
            "message:m9",
            "message:acc:INBOX:9",
            "import:2026-07-10",
            "linkedin:in/jmrd",
        ] {
            let l = LinkUpsert {
                provenance: Some(prov),
                ..derived(project("p1"), message("m1"), "includes", prov, 0.5)
            };
            assert!(db.upsert_link(l).is_ok(), "provenance {prov:?} should pass");
            db.unlink(project("p1"), message("m1"), "includes").unwrap();
        }
        // Free prose / copied content / control chars are rejected.
        for bad in [
            "Hi, let's meet Tuesday about the invoice",
            "see attached body text here",
            "message body: dear team\nregards",
            "no-colon-token",
            ":emptysource",
            "source:",
        ] {
            let l = LinkUpsert {
                provenance: Some(bad),
                ..derived(project("p1"), message("m1"), "includes", "message:m1", 0.5)
            };
            assert!(
                db.upsert_link(l).is_err(),
                "provenance {bad:?} must be rejected"
            );
        }
    }
}
