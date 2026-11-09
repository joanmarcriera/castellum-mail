// Castellum - Split Inbox (HP-2): tabs over the Inbox built from saved views.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// A split is a group smart mailbox (MM-1) whose scope carries
// `inbox_split: true` (stored in the existing `scope_json` column, so HP-2
// adds no table and no migration — DECISIONS.md 2026-10-05). The Inbox is
// partitioned into one bucket per split, in sidebar order, plus Other:
//
//   CASE WHEN (<split 0>) THEN 0 WHEN (<split 1>) THEN 1 ... ELSE -1 END
//
// A CASE takes the FIRST branch whose condition is true, and treats a NULL
// condition as not true, so every message gets exactly one bucket by
// construction: a message matching two splits belongs to the earlier one,
// a message matching none (or whose fields make a condition NULL) is in
// Other. The counts (one GROUP BY pass) and the lists (`bucket = ?`) are
// computed from the same expression over the same base rows, so they can
// never disagree. Each split's conditions are compiled by the one filter
// compiler (`filter_sql::compile_group` over `effective_group`), never a
// second WHERE builder.
//
// Base rows = messages in Inbox-role mailboxes (all of them, or the ones
// the caller names), snoozed messages left out — the same rows the Inbox
// list (`list_message_page` / `list_role_page`) shows.

use crate::filter_sql::{compile_group, has_header_leaf};
use crate::repo::{row_to_header, HEADER_COLUMNS};
use crate::smart_mailboxes::effective_group;
use crate::{Database, DbError};
use castellum_models::{InboxSplitCount, MessageHeader, SmartMailbox};
use rusqlite::{params_from_iter, types::Value, Connection};

/// Id (and bucket) of the remainder tab: mail no split claims.
pub const OTHER_SPLIT_ID: &str = "other";
/// Default English name of the remainder tab; the UI localises by id.
pub const OTHER_SPLIT_NAME: &str = "Other";

/// One split ready to run: the stored row and its compiled condition.
struct CompiledSplit {
    sm: SmartMailbox,
    where_sql: String,
    params: Vec<Value>,
}

/// `messages.mailbox_id` values the splits run over: every Inbox-role
/// mailbox when `requested` is empty, otherwise those of `requested` that
/// really are Inboxes (splits never apply to any other folder).
fn inbox_mailbox_ids(conn: &Connection, requested: &[String]) -> Result<Vec<String>, DbError> {
    let mut sql = "SELECT id FROM mailboxes WHERE role = 'inbox'".to_string();
    if !requested.is_empty() {
        sql.push_str(&format!(
            " AND id IN ({})",
            vec!["?"; requested.len()].join(", ")
        ));
    }
    sql.push_str(" ORDER BY id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(requested.iter()), |r| r.get(0))?;
    Ok(rows.collect::<Result<Vec<String>, _>>()?)
}

/// The bucket expression for `splits` (see the module comment), pushing
/// its bind values onto `params` in textual order.
fn bucket_expr(splits: &[CompiledSplit], params: &mut Vec<Value>) -> String {
    if splits.is_empty() {
        return "-1".to_string();
    }
    let mut sql = String::from("CASE");
    for (i, s) in splits.iter().enumerate() {
        sql.push_str(&format!(" WHEN ({}) THEN {i}", s.where_sql));
        params.extend(s.params.iter().cloned());
    }
    sql.push_str(" ELSE -1 END");
    sql
}

impl Database {
    /// The stored splits, sidebar order: group smart mailboxes flagged
    /// `scope.inbox_split`. Text-query mailboxes cannot be splits.
    pub fn list_inbox_splits(&self) -> Result<Vec<SmartMailbox>, DbError> {
        Ok(self
            .list_smart_mailboxes()?
            .into_iter()
            .filter(|sm| sm.group.is_some() && sm.scope.inbox_split)
            .collect())
    }

    /// Splits with their compiled conditions. A split whose stored
    /// conditions no longer compile (written by a newer build) is left out
    /// with a warning rather than breaking the whole Inbox; its mail then
    /// falls through to the next split or Other.
    fn compiled_splits(&self) -> Result<Vec<CompiledSplit>, DbError> {
        let mut out = Vec::new();
        for sm in self.list_inbox_splits()? {
            let Some(group) = &sm.group else { continue };
            // MM-1c slice 1: header conditions cannot split the Inbox (the
            // save path refuses them); a stored one is skipped like a split
            // that no longer compiles.
            if has_header_leaf(group) {
                tracing::warn!(
                    "inbox split {}: skipped, header conditions cannot split the Inbox",
                    sm.id
                );
                continue;
            }
            let mut params = Vec::new();
            match compile_group(&effective_group(group, &sm.scope), &mut params) {
                Ok(where_sql) => out.push(CompiledSplit {
                    sm,
                    where_sql,
                    params,
                }),
                Err(e) => tracing::warn!("inbox split {}: skipped, does not compile: {e}", sm.id),
            }
        }
        Ok(out)
    }

    /// Exact total / unread counts of every split, in tab order, then
    /// Other (always last, even with no splits). `mailbox_ids` picks the
    /// Inbox(es) shown; empty = every account's Inbox ("All Inboxes").
    pub fn inbox_split_counts(
        &self,
        mailbox_ids: &[String],
    ) -> Result<Vec<InboxSplitCount>, DbError> {
        let splits = self.compiled_splits()?;
        let mut buckets: Vec<InboxSplitCount> = splits
            .iter()
            .map(|s| InboxSplitCount {
                id: s.sm.id.clone(),
                name: s.sm.name.clone(),
                total: 0,
                unread: 0,
            })
            .chain(std::iter::once(InboxSplitCount {
                id: OTHER_SPLIT_ID.into(),
                name: OTHER_SPLIT_NAME.into(),
                total: 0,
                unread: 0,
            }))
            .collect();

        let conn = self.lock_conn();
        let inbox_ids = inbox_mailbox_ids(&conn, mailbox_ids)?;
        if inbox_ids.is_empty() {
            return Ok(buckets);
        }
        let mut params = Vec::new();
        let bucket = bucket_expr(&splits, &mut params);
        params.extend(inbox_ids.iter().cloned().map(Value::Text));
        let sql = format!(
            "SELECT bucket, COUNT(*), SUM(unread) FROM (
                SELECT {bucket} AS bucket, COALESCE(f.is_read, 0) = 0 AS unread
                FROM messages m
                LEFT JOIN message_flags f ON f.message_id = m.id
                LEFT JOIN snoozed_messages sn ON sn.message_id = m.id
                WHERE m.mailbox_id IN ({ids}) AND sn.message_id IS NULL
             ) GROUP BY bucket",
            ids = vec!["?"; inbox_ids.len()].join(", "),
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(params.iter()), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        let other = buckets.len() - 1;
        for row in rows {
            let (b, total, unread) = row?;
            let slot = usize::try_from(b)
                .ok()
                .filter(|&i| i < other)
                .unwrap_or(other);
            buckets[slot].total += total;
            buckets[slot].unread += unread;
        }
        Ok(buckets)
    }

    /// The messages of one tab (`split_id` = a split's id, or
    /// [`OTHER_SPLIT_ID`]), newest first, at most `limit`. `Ok(None)` when
    /// `split_id` is neither (the split was deleted or unflagged from
    /// another window); the caller shows Other instead of an error.
    pub fn inbox_split_messages(
        &self,
        mailbox_ids: &[String],
        split_id: &str,
        limit: i64,
    ) -> Result<Option<Vec<MessageHeader>>, DbError> {
        let splits = self.compiled_splits()?;
        let index: i64 = if split_id == OTHER_SPLIT_ID {
            -1
        } else {
            match splits.iter().position(|s| s.sm.id == split_id) {
                Some(i) => i as i64,
                None => return Ok(None),
            }
        };
        let mut rows = {
            let conn = self.lock_conn();
            let inbox_ids = inbox_mailbox_ids(&conn, mailbox_ids)?;
            if inbox_ids.is_empty() {
                return Ok(Some(Vec::new()));
            }
            let mut params: Vec<Value> = inbox_ids.iter().cloned().map(Value::Text).collect();
            let bucket = bucket_expr(&splits, &mut params);
            params.push(Value::Integer(index));
            params.push(Value::Integer(limit));
            let sql = format!(
                "SELECT {HEADER_COLUMNS}
                 FROM messages m
                 LEFT JOIN message_flags f ON f.message_id = m.id
                 LEFT JOIN threads t ON t.id = m.thread_id
                 LEFT JOIN message_bodies b ON b.message_id = m.id
                 LEFT JOIN snoozed_messages sn ON sn.message_id = m.id
                 WHERE m.mailbox_id IN ({ids}) AND sn.message_id IS NULL
                   AND ({bucket}) = ?
                 ORDER BY m.sent_at DESC, m.id
                 LIMIT ?",
                ids = vec!["?"; inbox_ids.len()].join(", "),
            );
            let mut stmt = conn.prepare(&sql)?;
            let mapped = stmt.query_map(params_from_iter(params.iter()), row_to_header)?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        self.attach_keywords(&mut rows)?;
        self.attach_recipients(&mut rows)?;
        Ok(Some(rows))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SmartMailboxUpsert;
    use castellum_models::{Account, Address, FilterGroup, Mailbox, NewMessage, SmartMailboxScope};
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_path() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "castellum-inbox-splits-test-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn mailbox(id: &str, role: Option<&str>) -> Mailbox {
        Mailbox {
            id: id.into(),
            account_id: "acc1".into(),
            path: id.trim_start_matches("acc1:").into(),
            name: id.trim_start_matches("acc1:").into(),
            role: role.map(Into::into),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        }
    }

    fn seed(db: &Database) {
        db.upsert_account(&Account {
            id: "acc1".into(),
            email: "me@local.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.upsert_mailbox(&mailbox("acc1:INBOX", Some("inbox")))
            .unwrap();
        db.upsert_mailbox(&mailbox("acc1:Archive", Some("archive")))
            .unwrap();
    }

    fn msg(db: &Database, mbox: &str, uid: i64, from: &str, subject: Option<&str>) -> String {
        let id = format!("{mbox}:{uid}");
        db.upsert_message(&NewMessage {
            id: id.clone(),
            account_id: "acc1".into(),
            mailbox_id: mbox.into(),
            provider_uid: Some(uid),
            rfc822_message_id: Some(format!("<{mbox}-{uid}@local.test>")),
            subject: subject.map(Into::into),
            from_name: None,
            from_address: Some(from.into()),
            sent_at: Some(1_800_000_000 + uid * 60),
            snippet: None,
            size_bytes: None,
            has_attachments: false,
            attachments: None,
            is_read: false,
            is_starred: false,
            addresses: vec![Address {
                kind: "from".into(),
                name: None,
                address: from.into(),
            }],
            text_body: None,
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: vec![],
        })
        .unwrap();
        id
    }

    fn group(field: &str, needle: &str) -> FilterGroup {
        serde_json::from_str(&format!(
            r#"{{"op":"and","conditions":[{{"field":"{field}","operator":"contains","value":"{needle}"}}],"groups":[]}}"#
        ))
        .unwrap()
    }

    fn split(db: &Database, name: &str, g: &FilterGroup) -> String {
        let scope = SmartMailboxScope {
            inbox_split: true,
            ..Default::default()
        };
        db.upsert_smart_mailbox(SmartMailboxUpsert {
            id: None,
            name,
            query: "",
            symbol: None,
            group: Some(g),
            scope: Some(&scope),
        })
        .unwrap()
        .id
    }

    fn ids(db: &Database, split_id: &str) -> Vec<String> {
        db.inbox_split_messages(&[], split_id, 500)
            .unwrap()
            .expect("known split")
            .into_iter()
            .map(|h| h.id)
            .collect()
    }

    /// Two overlapping splits: the Hetzner invoice matches both. Fixture
    /// ids: (both, hetzner-only, invoice-only, neither, NULL subject,
    /// archived, snoozed).
    struct Fixture {
        path: std::path::PathBuf,
        db: Database,
        invoices: String,
        hetzner: String,
        both: String,
        het_only: String,
        inv_only: String,
        neither: String,
        null_subject: String,
    }

    fn fixture() -> Fixture {
        let path = temp_path();
        let db = Database::open(&path).unwrap();
        seed(&db);
        let both = msg(
            &db,
            "acc1:INBOX",
            1,
            "billing@hetzner.example",
            Some("Invoice 42"),
        );
        let het_only = msg(
            &db,
            "acc1:INBOX",
            2,
            "status@hetzner.example",
            Some("Maintenance"),
        );
        let inv_only = msg(
            &db,
            "acc1:INBOX",
            3,
            "shop@example.org",
            Some("Your invoice"),
        );
        let neither = msg(&db, "acc1:INBOX", 4, "bob@local.test", Some("Lunch?"));
        // A NULL subject makes `subject LIKE …` NULL: it must still land
        // somewhere (Other), not vanish from every tab.
        let null_subject = msg(&db, "acc1:INBOX", 5, "carol@local.test", None);
        // Not Inbox mail / snoozed: in no tab at all, like the Inbox list.
        msg(
            &db,
            "acc1:Archive",
            6,
            "billing@hetzner.example",
            Some("Invoice 41"),
        );
        let snoozed = msg(
            &db,
            "acc1:INBOX",
            7,
            "billing@hetzner.example",
            Some("Invoice 43"),
        );
        db.snooze_message(&snoozed, 4_000_000_000).unwrap();
        let invoices = split(&db, "Invoices", &group("subject", "invoice"));
        let hetzner = split(&db, "Hetzner", &group("from", "hetzner"));
        Fixture {
            path,
            db,
            invoices,
            hetzner,
            both,
            het_only,
            inv_only,
            neither,
            null_subject,
        }
    }

    #[test]
    fn overlapping_splits_put_each_message_in_exactly_one_tab() {
        let f = fixture();
        let a = ids(&f.db, &f.invoices);
        let b = ids(&f.db, &f.hetzner);
        let other = ids(&f.db, OTHER_SPLIT_ID);

        assert_eq!(
            a,
            vec![f.inv_only.clone(), f.both.clone()],
            "first split wins the overlap"
        );
        assert_eq!(
            b,
            vec![f.het_only.clone()],
            "the overlap is not repeated in the second split"
        );
        assert_eq!(other, vec![f.null_subject.clone(), f.neither.clone()]);

        // Exactly-one: no id twice across tabs, and the tabs together are
        // the whole Inbox list (archived and snoozed mail excluded).
        let all: Vec<&String> = a.iter().chain(&b).chain(&other).collect();
        let unique: HashSet<&String> = all.iter().copied().collect();
        assert_eq!(all.len(), unique.len(), "a message appeared in two tabs");
        let inbox: HashSet<String> =
            f.db.list_message_page("acc1", "acc1:INBOX", None, 500)
                .unwrap()
                .rows
                .into_iter()
                .map(|h| h.id)
                .collect();
        assert_eq!(unique.into_iter().cloned().collect::<HashSet<_>>(), inbox);
        let _ = std::fs::remove_file(&f.path);
    }

    #[test]
    fn counts_match_the_lists_and_track_unread() {
        let f = fixture();
        // Local flag only (mark_read is gated on write mode).
        f.db.lock_conn()
            .execute(
                "UPDATE message_flags SET is_read = 1 WHERE message_id = ?1",
                [&f.both],
            )
            .unwrap();
        let counts = f.db.inbox_split_counts(&[]).unwrap();
        assert_eq!(
            counts.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec![f.invoices.as_str(), f.hetzner.as_str(), OTHER_SPLIT_ID]
        );
        assert_eq!(counts[2].name, OTHER_SPLIT_NAME);
        for c in &counts {
            let rows = f.db.inbox_split_messages(&[], &c.id, 500).unwrap().unwrap();
            assert_eq!(c.total, rows.len() as i64, "total of {}", c.name);
            assert_eq!(
                c.unread,
                rows.iter().filter(|h| !h.is_read).count() as i64,
                "unread of {}",
                c.name
            );
        }
        assert_eq!((counts[0].total, counts[0].unread), (2, 1));
        let _ = std::fs::remove_file(&f.path);
    }

    #[test]
    fn reordering_splits_moves_the_overlap_to_the_new_first_split() {
        let f = fixture();
        f.db.reorder_smart_mailboxes(&[f.hetzner.clone(), f.invoices.clone()])
            .unwrap();
        assert_eq!(
            ids(&f.db, &f.hetzner),
            vec![f.het_only.clone(), f.both.clone()]
        );
        assert_eq!(ids(&f.db, &f.invoices), vec![f.inv_only.clone()]);
        let _ = std::fs::remove_file(&f.path);
    }

    #[test]
    fn plain_smart_mailboxes_are_not_splits_and_no_splits_means_all_in_other() {
        let path = temp_path();
        let db = Database::open(&path).unwrap();
        seed(&db);
        let a = msg(
            &db,
            "acc1:INBOX",
            1,
            "billing@hetzner.example",
            Some("Invoice"),
        );
        let g = group("subject", "invoice");
        db.upsert_smart_mailbox(SmartMailboxUpsert {
            id: None,
            name: "Not a split",
            query: "",
            symbol: None,
            group: Some(&g),
            scope: None,
        })
        .unwrap();
        assert!(db.list_inbox_splits().unwrap().is_empty());
        let counts = db.inbox_split_counts(&[]).unwrap();
        assert_eq!(counts.len(), 1);
        assert_eq!(
            (counts[0].id.as_str(), counts[0].total),
            (OTHER_SPLIT_ID, 1)
        );
        assert_eq!(ids(&db, OTHER_SPLIT_ID), vec![a]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unknown_split_is_none_and_non_inbox_ids_are_ignored() {
        let f = fixture();
        assert!(f
            .db
            .inbox_split_messages(&[], "gone", 500)
            .unwrap()
            .is_none());
        // Asking for the Archive: not an Inbox, so the tabs are empty.
        let archive = ["acc1:Archive".to_string()];
        let counts = f.db.inbox_split_counts(&archive).unwrap();
        assert!(counts.iter().all(|c| c.total == 0));
        // Naming the Inbox explicitly gives the same answer as "all Inboxes".
        let inbox = ["acc1:INBOX".to_string()];
        assert_eq!(
            f.db.inbox_split_counts(&inbox).unwrap(),
            f.db.inbox_split_counts(&[]).unwrap()
        );
        let _ = std::fs::remove_file(&f.path);
    }

    #[test]
    fn splits_survive_reopening_the_profile() {
        let f = fixture();
        let path = f.path.clone();
        let before = f.db.inbox_split_counts(&[]).unwrap();
        drop(f.db);
        let db = Database::open(&path).unwrap();
        let names: Vec<String> = db
            .list_inbox_splits()
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["Invoices", "Hetzner"]);
        assert_eq!(db.inbox_split_counts(&[]).unwrap(), before);
        let _ = std::fs::remove_file(&path);
    }
    /// SHOULD 6 (review): header conditions cannot split the Inbox in slice 1.
    /// The save path refuses them (with the editor's reason), and a split
    /// stored anyway (older build, hand edit) is skipped, never evaluated.
    #[test]
    fn header_conditions_cannot_split_the_inbox() {
        let path = temp_path();
        let db = Database::open(&path).unwrap();
        seed(&db);
        let a = msg(&db, "acc1:INBOX", 1, "x@lists.example", Some("Hi"));
        let header: FilterGroup = serde_json::from_str(
            r#"{"op":"and","conditions":[{"field":{"header":"List-Id"},"operator":"is","value":true}],"groups":[]}"#,
        )
        .unwrap();
        let nested = FilterGroup {
            op: castellum_models::BoolOp::Or,
            conditions: group("subject", "hi").conditions,
            groups: vec![header.clone()],
        };
        let scope = SmartMailboxScope {
            inbox_split: true,
            ..Default::default()
        };
        for g in [&header, &nested] {
            let err = db
                .upsert_smart_mailbox(SmartMailboxUpsert {
                    id: None,
                    name: "Lists",
                    query: "",
                    symbol: None,
                    group: Some(g),
                    scope: Some(&scope),
                })
                .unwrap_err();
            assert!(err.0.contains("cannot split the Inbox"), "{err:?}");
        }
        // As a plain smart mailbox (no split) the same group saves fine.
        db.upsert_smart_mailbox(SmartMailboxUpsert {
            id: None,
            name: "Lists",
            query: "",
            symbol: None,
            group: Some(&header),
            scope: None,
        })
        .unwrap();
        // A stored header split is skipped: everything stays in Other.
        let scope_json = serde_json::to_string(&scope).unwrap();
        db.lock_conn()
            .execute(
                "UPDATE smart_mailboxes SET scope_json = ?1 WHERE name = 'Lists'",
                rusqlite::params![scope_json],
            )
            .unwrap();
        assert_eq!(db.list_inbox_splits().unwrap().len(), 1);
        let counts = db.inbox_split_counts(&[]).unwrap();
        assert_eq!(counts.len(), 1);
        assert_eq!(counts[0].id, OTHER_SPLIT_ID);
        assert_eq!(ids(&db, OTHER_SPLIT_ID), vec![a]);
        let _ = std::fs::remove_file(&path);
    }
}
