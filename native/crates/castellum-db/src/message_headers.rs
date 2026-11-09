// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//! Stored message headers (migration 0057, MM-1c / #2710).
//!
//! Usage: `replace_message_headers(id, &headers, source, false)` after a body is
//! hydrated; `message_header_coverage()` for "headers known for N of M";
//! `list_known_header_names(limit)` for editor completion. Smart-mailbox
//! `Header` conditions read the tables through `filter_sql::header_sql`.
//!
//! Invariants: header values are mail content (never exported, logged or put
//! in diagnostics); `replace` is a full replace in one transaction, never a
//! merge; a capture row exists even when no header was kept, so "absent"
//! can be told from "unknown"; the name dictionary is capped, and one message
//! can add at most MAX_NEW_HEADER_NAMES_PER_MESSAGE names, so a hostile sender
//! can neither grow it without bound nor fill it on their own; names a saved
//! smart mailbox, filter or rule refers to are always admitted.

use std::collections::HashSet;

use rusqlite::{params, Connection};

use castellum_models::{
    header_name_status, normalize_header_name, HeaderCaptureSource, HeaderCoverage,
    HeaderNameStatus, StoredHeader,
};

use crate::{Database, DbError};

/// Most distinct header names the dictionary will hold. New names beyond this
/// are dropped (their rows are skipped, the capture is partial); existing
/// names, and names a saved condition refers to, keep working.
pub const MAX_HEADER_NAMES: i64 = 5000;

/// Most NEW dictionary names one message may add. A message with more unseen
/// names keeps the first ones and is a partial capture: one sender cannot
/// exhaust the dictionary with a few messages full of invented names.
pub const MAX_NEW_HEADER_NAMES_PER_MESSAGE: usize = 8;

/// Normalised header names that saved smart mailboxes, filters and rules
/// refer to (`{"header": "<name>"}` anywhere in their condition trees). They
/// are admitted to the dictionary regardless of its caps, so a sender flood
/// can never stop a configured condition from seeing its header. Invalid JSON
/// rows are skipped (`json_valid`), never an error.
fn referenced_header_names(conn: &Connection) -> Result<HashSet<String>, DbError> {
    let mut stmt = conn.prepare_cached(
        "SELECT j.value FROM smart_mailboxes s, json_tree(s.group_json) j
         WHERE s.group_json IS NOT NULL AND json_valid(s.group_json)
           AND j.key = 'header' AND j.type = 'text'
         UNION
         SELECT j.value FROM saved_filters f, json_tree(f.group_json) j
         WHERE json_valid(f.group_json) AND j.key = 'header' AND j.type = 'text'",
    )?;
    let names = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .filter_map(|n| normalize_header_name(&n))
        .collect();
    Ok(names)
}

impl Database {
    /// Replace the stored headers of one message and mark it captured, in one
    /// transaction. A message id that no longer exists is a no-op (the row may
    /// have been expunged between fetch and persist).
    ///
    /// `truncated` is the extractor's "I dropped something" flag (any cap hit,
    /// input cut, or parse failure). It is OR-ed with the store's own drops
    /// (name dictionary full) into the capture row's `partial` flag, and a
    /// partial capture is UNKNOWN for "absent" and negated conditions (fail
    /// closed). Pass `false` only when the set is known to be complete.
    pub fn replace_message_headers(
        &self,
        message_id: &str,
        headers: &[StoredHeader],
        source: HeaderCaptureSource,
        truncated: bool,
    ) -> Result<(), DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM messages WHERE id = ?1)",
            params![message_id],
            |r| r.get(0),
        )?;
        if !exists {
            return Ok(());
        }
        tx.execute(
            "DELETE FROM message_headers WHERE message_id = ?1",
            params![message_id],
        )?;
        let mut partial = truncated;
        {
            let mut name_count: i64 =
                tx.query_row("SELECT COUNT(*) FROM header_names", [], |r| r.get(0))?;
            // Loaded only when this message brings a name the dictionary lacks.
            let mut referenced: Option<HashSet<String>> = None;
            let mut new_names = 0usize;
            let mut find = tx.prepare_cached("SELECT id FROM header_names WHERE name = ?1")?;
            let mut add = tx.prepare_cached("INSERT INTO header_names (name) VALUES (?1)")?;
            let mut ins = tx.prepare_cached(
                "INSERT OR REPLACE INTO message_headers (message_id, name_id, ordinal, value)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for h in headers {
                let id: Option<i64> = find
                    .query_row(params![h.name], |r| r.get(0))
                    .map(Some)
                    .or_else(|e| match e {
                        rusqlite::Error::QueryReturnedNoRows => Ok(None),
                        other => Err(other),
                    })?;
                let id = match id {
                    Some(id) => id,
                    None => {
                        if referenced.is_none() {
                            referenced = Some(referenced_header_names(&tx)?);
                        }
                        let wanted = referenced.as_ref().is_some_and(|r| r.contains(&h.name));
                        // Every occurrence of a name shares one admission:
                        // `find` sees the name once it was added.
                        if !wanted
                            && (name_count >= MAX_HEADER_NAMES
                                || new_names >= MAX_NEW_HEADER_NAMES_PER_MESSAGE)
                        {
                            partial = true;
                            continue;
                        }
                        add.execute(params![h.name])?;
                        name_count += 1;
                        if !wanted {
                            new_names += 1;
                        }
                        tx.last_insert_rowid()
                    }
                };
                ins.execute(params![message_id, id, h.ordinal, h.value])?;
            }
        }
        tx.execute(
            "INSERT INTO message_header_capture (message_id, source, partial) VALUES (?1, ?2, ?3)
             ON CONFLICT(message_id) DO UPDATE SET
                 source = excluded.source, captured_at = datetime('now'),
                 partial = excluded.partial",
            params![message_id, source.as_str(), partial],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The stored `(name, ordinal, value)` rows of one message, name then
    /// ordinal order. Mail content: for tests and the editor-free core only;
    /// never log or export the result.
    pub fn list_stored_header_rows(
        &self,
        message_id: &str,
    ) -> Result<Vec<(String, u32, String)>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT n.name, h.ordinal, h.value FROM message_headers h
             JOIN header_names n ON n.id = h.name_id
             WHERE h.message_id = ?1 ORDER BY n.name, h.ordinal",
        )?;
        let rows = stmt
            .query_map(params![message_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Messages whose headers are fully known (a COMPLETE capture) vs all
    /// messages. A partial capture is not counted: for "absent" and negated
    /// conditions it is as unknown as no capture at all. Local-only.
    pub fn message_header_coverage(&self) -> Result<HeaderCoverage, DbError> {
        let conn = self.lock_conn();
        let (captured, total) = conn.query_row(
            "SELECT (SELECT COUNT(*) FROM message_header_capture WHERE partial = 0),
                    (SELECT COUNT(*) FROM messages)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(HeaderCoverage { captured, total })
    }

    /// Remove every stored row whose header name the current policy no longer
    /// keeps (denylisted, built-in or invalid), and the names themselves.
    /// Called once per `Database::open`, after migrations: it walks only the
    /// name dictionary (at most MAX_HEADER_NAMES rows, classified in Rust, the
    /// single policy source) and deletes through `idx_message_headers_name`,
    /// so it is cheap, idempotent, and re-applies itself whenever a release
    /// widens the denylist. Returns how many names were purged.
    pub(crate) fn purge_denied_header_names(&self) -> Result<usize, DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let denied: Vec<i64> = {
            let mut stmt = tx.prepare("SELECT id, name FROM header_names")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter()
                .filter(|(_, n)| header_name_status(n) != HeaderNameStatus::Storable)
                .map(|(id, _)| id)
                .collect()
        };
        for id in &denied {
            // The AFTER DELETE trigger drops the name with its last row; the
            // explicit delete covers a name that had no rows left.
            tx.execute(
                "DELETE FROM message_headers WHERE name_id = ?1",
                params![id],
            )?;
            tx.execute("DELETE FROM header_names WHERE id = ?1", params![id])?;
        }
        tx.commit()?;
        Ok(denied.len())
    }

    /// Up to `limit` lower-cased header names that currently have at least one
    /// stored value, alphabetical. For editor completion only.
    pub fn list_known_header_names(&self, limit: usize) -> Result<Vec<String>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT n.name FROM header_names n
             WHERE EXISTS (SELECT 1 FROM message_headers h WHERE h.name_id = n.id)
             ORDER BY n.name LIMIT ?1",
        )?;
        let names = stmt
            .query_map(params![limit as i64], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use castellum_models::{Account, Address, Mailbox, NewMessage};

    fn db() -> Database {
        // A per-process counter, not only the clock: macOS clocks tick in
        // microseconds, so two parallel tests could otherwise share a path and
        // race each other's migrations.
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-msg-headers-{}-{}-{}.db",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Database::open(&path).expect("open")
    }

    fn seed(db: &Database, uids: &[i64]) {
        db.upsert_account(&Account {
            id: "a".into(),
            email: "me@local.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.upsert_mailbox(&Mailbox {
            id: "a:INBOX".into(),
            account_id: "a".into(),
            path: "INBOX".into(),
            name: "INBOX".into(),
            role: Some("inbox".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        for uid in uids {
            db.upsert_message(&NewMessage {
                id: format!("a:INBOX:{uid}"),
                account_id: "a".into(),
                mailbox_id: "a:INBOX".into(),
                provider_uid: Some(*uid),
                rfc822_message_id: None,
                subject: Some("s".into()),
                from_name: None,
                from_address: Some("x@y.test".into()),
                sent_at: Some(1_800_000_000),
                snippet: None,
                size_bytes: None,
                has_attachments: false,
                attachments: None,
                is_read: false,
                is_starred: false,
                addresses: vec![Address {
                    kind: "from".into(),
                    name: None,
                    address: "x@y.test".into(),
                }],
                text_body: None,
                sanitized_html: None,
                html_body: None,
                has_remote_content: false,
                references: vec![],
            })
            .unwrap();
        }
    }

    fn h(name: &str, ordinal: u32, value: &str) -> StoredHeader {
        StoredHeader {
            name: name.into(),
            ordinal,
            value: value.into(),
        }
    }

    fn rows(db: &Database, id: &str) -> i64 {
        db.lock_conn()
            .query_row(
                "SELECT COUNT(*) FROM message_headers WHERE message_id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn replace_replaces_not_merges_and_marks_captured() {
        let db = db();
        seed(&db, &[1]);
        let b = HeaderCaptureSource::Body;
        db.replace_message_headers(
            "a:INBOX:1",
            &[h("list-id", 0, "x"), h("x-a", 0, "1")],
            b,
            false,
        )
        .unwrap();
        assert_eq!(rows(&db, "a:INBOX:1"), 2);
        // Re-hydration yielding fewer headers replaces the old set.
        db.replace_message_headers("a:INBOX:1", &[h("list-id", 0, "y")], b, false)
            .unwrap();
        assert_eq!(rows(&db, "a:INBOX:1"), 1);
        assert_eq!(
            db.message_header_coverage().unwrap(),
            HeaderCoverage {
                captured: 1,
                total: 1
            }
        );
        // Zero headers still marks the message captured.
        db.replace_message_headers("a:INBOX:1", &[], b, false)
            .unwrap();
        assert_eq!(rows(&db, "a:INBOX:1"), 0);
        assert_eq!(db.message_header_coverage().unwrap().captured, 1);
    }

    #[test]
    fn unknown_message_is_a_noop_and_delete_cascades() {
        let db = db();
        seed(&db, &[1, 2]);
        let b = HeaderCaptureSource::Body;
        db.replace_message_headers("a:INBOX:99", &[h("x-a", 0, "1")], b, false)
            .unwrap();
        assert_eq!(db.message_header_coverage().unwrap().captured, 0);
        db.replace_message_headers("a:INBOX:1", &[h("x-a", 0, "1")], b, false)
            .unwrap();
        db.replace_message_headers("a:INBOX:2", &[h("x-a", 0, "2")], b, false)
            .unwrap();
        db.lock_conn()
            .execute("DELETE FROM messages WHERE id = 'a:INBOX:1'", [])
            .unwrap();
        assert_eq!(rows(&db, "a:INBOX:1"), 0);
        assert_eq!(
            db.message_header_coverage().unwrap(),
            HeaderCoverage {
                captured: 1,
                total: 1
            }
        );
    }

    #[test]
    fn known_names_lists_only_names_with_values() {
        let db = db();
        seed(&db, &[1]);
        let b = HeaderCaptureSource::Body;
        db.replace_message_headers(
            "a:INBOX:1",
            &[h("x-b", 0, "1"), h("list-id", 0, "2")],
            b,
            false,
        )
        .unwrap();
        db.replace_message_headers("a:INBOX:1", &[h("x-b", 0, "1")], b, false)
            .unwrap();
        assert_eq!(db.list_known_header_names(10).unwrap(), vec!["x-b"]);
        assert_eq!(db.list_known_header_names(0).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn dictionary_is_capped_against_hostile_name_floods() {
        let db = db();
        seed(&db, &[1]);
        let many: Vec<StoredHeader> = (0..64).map(|i| h(&format!("x-n{i}"), 0, "v")).collect();
        db.lock_conn()
            .execute_batch(&format!(
                "WITH RECURSIVE c(i) AS (SELECT 0 UNION ALL SELECT i+1 FROM c WHERE i < {})
                 INSERT INTO header_names (name) SELECT 'filler' || i FROM c;",
                MAX_HEADER_NAMES - 1
            ))
            .unwrap();
        db.replace_message_headers("a:INBOX:1", &many, HeaderCaptureSource::Body, false)
            .unwrap();
        assert_eq!(
            rows(&db, "a:INBOX:1"),
            0,
            "dictionary full: new names dropped"
        );
        let n: i64 = db
            .lock_conn()
            .query_row("SELECT COUNT(*) FROM header_names", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, MAX_HEADER_NAMES);
    }

    #[test]
    fn names_with_no_remaining_rows_are_removed_on_replace_and_delete() {
        let db = db();
        seed(&db, &[1]);
        let b = HeaderCaptureSource::Body;
        db.replace_message_headers("a:INBOX:1", &[h("x-a", 0, "1"), h("x-b", 0, "2")], b, false)
            .unwrap();
        db.replace_message_headers("a:INBOX:1", &[h("x-a", 0, "1")], b, false)
            .unwrap();
        let names = |db: &Database| -> Vec<String> {
            let c = db.lock_conn();
            let mut st = c
                .prepare("SELECT name FROM header_names ORDER BY name")
                .unwrap();
            st.query_map([], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        assert_eq!(names(&db), vec!["x-a"], "x-b orphaned by the replace");
        db.lock_conn()
            .execute("DELETE FROM messages WHERE id = 'a:INBOX:1'", [])
            .unwrap();
        assert!(
            names(&db).is_empty(),
            "cascade delete removes the last name"
        );
    }
    fn is_partial(db: &Database, id: &str) -> bool {
        db.lock_conn()
            .query_row(
                "SELECT partial FROM message_header_capture WHERE message_id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// SHOULD 4 (review): one message adds at most
    /// MAX_NEW_HEADER_NAMES_PER_MESSAGE new names (overflow = partial), names
    /// already known are free, and a name a saved smart mailbox or rule refers
    /// to is always admitted, even with the dictionary full.
    #[test]
    fn new_names_per_message_are_capped_but_referenced_names_always_admitted() {
        let db = db();
        seed(&db, &[1, 2, 3]);
        let b = HeaderCaptureSource::Body;
        let flood: Vec<StoredHeader> = (0..20).map(|i| h(&format!("x-f{i}"), 0, "v")).collect();
        db.replace_message_headers("a:INBOX:1", &flood, b, false)
            .unwrap();
        assert_eq!(
            rows(&db, "a:INBOX:1"),
            MAX_NEW_HEADER_NAMES_PER_MESSAGE as i64
        );
        assert!(
            is_partial(&db, "a:INBOX:1"),
            "overflow names make it partial"
        );
        // Repeats of an admitted name do not count as new names.
        let reps: Vec<StoredHeader> = (0..8)
            .map(|i| h("x-rep", i, "v"))
            .chain((0..7).map(|i| h(&format!("x-g{i}"), 0, "v")))
            .collect();
        db.replace_message_headers("a:INBOX:2", &reps, b, false)
            .unwrap();
        assert_eq!(rows(&db, "a:INBOX:2"), 15);
        assert!(!is_partial(&db, "a:INBOX:2"));

        // A smart mailbox and a rule refer to two names (as typed by the user).
        db.lock_conn()
            .execute_batch(
                r#"INSERT INTO smart_mailboxes (id, name, query, sort_order, created_at, updated_at, group_json)
                   VALUES ('sm', 'S', '', 0, 0, 0,
                   '{"op":"and","conditions":[{"field":{"header":"List-Id"},"operator":"is","value":true}],"groups":[]}');"#,
            )
            .unwrap();
        let rule_group = r#"{"op":"or","conditions":[],"groups":[{"op":"and","conditions":[{"field":{"header":"X-GitHub-Reason:"},"operator":"contains","value":"x"}],"groups":[]}]}"#;
        db.lock_conn()
            .execute(
                "INSERT INTO saved_filters (id, name, kind, group_json, trigger_kind)
                 VALUES ('r', 'R', 'rule', ?1, 'manual')",
                params![rule_group],
            )
            .unwrap();
        // Fill the dictionary to the cap.
        db.lock_conn()
            .execute_batch(&format!(
                "WITH RECURSIVE c(i) AS (SELECT 0 UNION ALL SELECT i+1 FROM c WHERE i < {})
                 INSERT INTO header_names (name) SELECT 'filler' || i FROM c;",
                MAX_HEADER_NAMES
            ))
            .unwrap();
        // Ten unseen names first, then the two referenced ones.
        let hs: Vec<StoredHeader> = (0..10)
            .map(|i| h(&format!("x-z{i}"), 0, "v"))
            .chain([h("list-id", 0, "dev"), h("x-github-reason", 0, "mention")])
            .collect();
        db.replace_message_headers("a:INBOX:3", &hs, b, false)
            .unwrap();
        let kept: Vec<String> = db
            .list_stored_header_rows("a:INBOX:3")
            .unwrap()
            .into_iter()
            .map(|(n, _, _)| n)
            .collect();
        assert_eq!(kept, ["list-id", "x-github-reason"]);
        assert!(is_partial(&db, "a:INBOX:3"));
    }

    /// NIT (review): coverage counts COMPLETE captures only.
    #[test]
    fn coverage_does_not_count_partial_captures() {
        let db = db();
        seed(&db, &[1, 2]);
        let b = HeaderCaptureSource::Body;
        db.replace_message_headers("a:INBOX:1", &[h("x-a", 0, "1")], b, false)
            .unwrap();
        db.replace_message_headers("a:INBOX:2", &[h("x-a", 0, "1")], b, true)
            .unwrap();
        assert_eq!(
            db.message_header_coverage().unwrap(),
            HeaderCoverage {
                captured: 1,
                total: 2
            }
        );
    }

    /// SHOULD 5 (review): rows stored under a name the policy now denies are
    /// purged (once per open), storable names are untouched.
    #[test]
    fn denied_header_names_are_purged() {
        let db = db();
        seed(&db, &[1]);
        db.replace_message_headers(
            "a:INBOX:1",
            &[
                h("x-shopify-hmac-sha256", 0, "abc"),
                h("x-session-id", 0, "s"),
                h("list-id", 0, "dev"),
            ],
            HeaderCaptureSource::Body,
            false,
        )
        .unwrap();
        // Also a dictionary name with no rows left.
        db.lock_conn()
            .execute("INSERT INTO header_names (name) VALUES ('x-auth-user')", [])
            .unwrap();
        assert_eq!(db.purge_denied_header_names().unwrap(), 3);
        let kept: Vec<String> = db
            .list_stored_header_rows("a:INBOX:1")
            .unwrap()
            .into_iter()
            .map(|(n, _, v)| format!("{n}={v}"))
            .collect();
        assert_eq!(kept, ["list-id=dev"]);
        let names: i64 = db
            .lock_conn()
            .query_row("SELECT COUNT(*) FROM header_names", [], |r| r.get(0))
            .unwrap();
        assert_eq!(names, 1);
        assert_eq!(db.purge_denied_header_names().unwrap(), 0, "idempotent");
    }
}
