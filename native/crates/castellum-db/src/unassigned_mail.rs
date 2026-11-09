// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//! "Unassigned mail" (AW3): inbox threads that have neither a project link
//! nor a work-item link.
//!
//! Read-only and ids-only: it returns thread ids with counts and the newest
//! timestamp, never message rows (the list UI fetches rows through the
//! existing message-list API). Link state comes from the `links` table, in
//! either direction, at thread level AND at message level (a project
//! "includes" individual messages, so a thread whose message is filed under
//! a project is assigned). Every probe is an index lookup (`idx_links_dst` /
//! `idx_links_src` / `idx_messages_thread`) per inbox thread, never a scan of
//! `links`.

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::{Database, DbError};

/// One unassigned inbox thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnassignedThread {
    pub thread_id: String,
    /// Messages of the thread that sit in an inbox.
    pub message_count: i64,
    /// Newest `received_at` (falling back to `sent_at`) among them, epoch s.
    pub last_message_at: Option<i64>,
}

/// One page of [`Database::list_unassigned_mail_threads`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnassignedMailPage {
    pub threads: Vec<UnassignedThread>,
    /// Unassigned inbox threads in all, independent of paging.
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// Inbox threads, grouped, with the "no project / work-item link" predicate.
const UNASSIGNED_CTE: &str = "WITH inbox AS (
        SELECT m.thread_id AS thread_id,
               COUNT(*) AS n,
               MAX(COALESCE(m.received_at, m.sent_at)) AS last_at
        FROM messages m
        JOIN mailboxes mb ON mb.id = m.mailbox_id AND mb.role = 'inbox'
        WHERE m.thread_id IS NOT NULL
        GROUP BY m.thread_id
    ),
    unassigned AS (
        SELECT thread_id, n, last_at FROM inbox t
        WHERE NOT EXISTS (
                SELECT 1 FROM links l INDEXED BY idx_links_dst
                WHERE l.dst_type = 'thread' AND l.dst_id = t.thread_id
                  AND l.src_type IN ('project', 'work_item'))
          AND NOT EXISTS (
                SELECT 1 FROM links l INDEXED BY idx_links_src
                WHERE l.src_type = 'thread' AND l.src_id = t.thread_id
                  AND l.dst_type IN ('project', 'work_item'))
          AND NOT EXISTS (
                SELECT 1 FROM messages mm
                JOIN links l INDEXED BY idx_links_dst
                  ON l.dst_type = 'message' AND l.dst_id = mm.id
                 AND l.src_type IN ('project', 'work_item')
                WHERE mm.thread_id = t.thread_id)
          AND NOT EXISTS (
                SELECT 1 FROM messages mm
                JOIN links l INDEXED BY idx_links_src
                  ON l.src_type = 'message' AND l.src_id = mm.id
                 AND l.dst_type IN ('project', 'work_item')
                WHERE mm.thread_id = t.thread_id)
    )";

/// Largest page one call returns.
pub const UNASSIGNED_MAX_LIMIT: i64 = 500;

impl Database {
    /// Unassigned inbox threads, newest first (then thread id), one page.
    /// `limit` is clamped to 1..=500, a negative `offset` to 0.
    pub fn list_unassigned_mail_threads(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<UnassignedMailPage, DbError> {
        let limit = limit.clamp(1, UNASSIGNED_MAX_LIMIT);
        let offset = offset.max(0);
        let conn = self.lock_conn();
        let total: i64 = conn.query_row(
            &format!("{UNASSIGNED_CTE} SELECT COUNT(*) FROM unassigned"),
            [],
            |r| r.get(0),
        )?;
        let mut stmt = conn.prepare(&format!(
            "{UNASSIGNED_CTE}
             SELECT thread_id, n, last_at FROM unassigned
             ORDER BY (last_at IS NULL), last_at DESC, thread_id
             LIMIT ?1 OFFSET ?2"
        ))?;
        let rows = stmt.query_map(params![limit, offset], |r| {
            Ok(UnassignedThread {
                thread_id: r.get(0)?,
                message_count: r.get(1)?,
                last_message_at: r.get(2)?,
            })
        })?;
        Ok(UnassignedMailPage {
            threads: rows.collect::<Result<Vec<_>, _>>()?,
            total,
            limit,
            offset,
        })
    }
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
            "castellum-unassigned-test-{}-{}.db",
            std::process::id(),
            n
        ));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
        let db = Database::open(&path).expect("open");
        db.upsert_account(&Account {
            id: "acc".into(),
            email: "me@local.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        for (path, role) in [("INBOX", Some("inbox")), ("Sent", Some("sent"))] {
            db.upsert_mailbox(&Mailbox {
                id: format!("acc:{path}"),
                account_id: "acc".into(),
                path: path.into(),
                name: path.into(),
                role: role.map(String::from),
                delimiter: Some("/".into()),
                subscribed: true,
                selectable: true,
                sync_by_default: true,
            })
            .unwrap();
        }
        db
    }

    /// Insert a message and force its thread id (threading is not under test).
    fn msg(db: &Database, box_: &str, uid: i64, thread: &str, at: i64) -> String {
        let id = format!("acc:{box_}:{uid}");
        db.upsert_message(&NewMessage {
            id: id.clone(),
            account_id: "acc".into(),
            mailbox_id: format!("acc:{box_}"),
            provider_uid: Some(uid),
            rfc822_message_id: Some(format!("<{uid}@{box_}.test>")),
            subject: Some("S".into()),
            from_name: None,
            from_address: Some("s@local.test".into()),
            sent_at: Some(at),
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
        db.lock_conn()
            .execute(
                "UPDATE messages SET thread_id = ?2 WHERE id = ?1",
                params![id, thread],
            )
            .unwrap();
        id
    }

    fn raw_link(db: &Database, src: (&str, &str), dst: (&str, &str)) {
        db.lock_conn()
            .execute(
                "INSERT INTO links (src_type, src_id, dst_type, dst_id, relation, origin,
                                    created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 'about', 'manual', 1, 1)",
                params![src.0, src.1, dst.0, dst.1],
            )
            .unwrap();
    }

    fn ids(p: &UnassignedMailPage) -> Vec<&str> {
        p.threads.iter().map(|t| t.thread_id.as_str()).collect()
    }

    #[test]
    fn lists_only_inbox_threads_without_project_or_work_item_links() {
        let db = fresh_db();
        msg(&db, "INBOX", 1, "t-free-old", 100);
        msg(&db, "INBOX", 2, "t-free-new", 300);
        msg(&db, "INBOX", 3, "t-free-new", 350); // 2 inbox messages
        msg(&db, "INBOX", 4, "t-project-thread", 400);
        msg(&db, "INBOX", 5, "t-project-msg", 500);
        let wi_msg = msg(&db, "INBOX", 6, "t-workitem-msg", 600);
        msg(&db, "INBOX", 7, "t-workitem-thread", 700);
        msg(&db, "INBOX", 8, "t-reverse", 800);
        msg(&db, "INBOX", 9, "t-contact-only", 900);
        msg(&db, "Sent", 1, "t-sent-only", 1000);
        let pm = format!("acc:INBOX:{}", 5);

        raw_link(&db, ("project", "p1"), ("thread", "t-project-thread"));
        raw_link(&db, ("project", "p1"), ("message", &pm));
        raw_link(&db, ("work_item", "w1"), ("message", &wi_msg));
        raw_link(&db, ("work_item", "w2"), ("thread", "t-workitem-thread"));
        raw_link(&db, ("thread", "t-reverse"), ("project", "p1"));
        // A contact link is NOT an assignment.
        raw_link(&db, ("thread", "t-contact-only"), ("contact", "1"));

        let page = db.list_unassigned_mail_threads(50, 0).unwrap();
        assert_eq!(
            ids(&page),
            ["t-contact-only", "t-free-new", "t-free-old"],
            "newest first; linked and non-inbox threads excluded"
        );
        assert_eq!(page.total, 3);
        let new = &page.threads[1];
        assert_eq!((new.message_count, new.last_message_at), (2, Some(350)));
    }

    #[test]
    fn a_link_makes_a_thread_drop_out_and_an_unlink_brings_it_back() {
        let db = fresh_db();
        msg(&db, "INBOX", 1, "t1", 100);
        assert_eq!(db.list_unassigned_mail_threads(10, 0).unwrap().total, 1);
        raw_link(&db, ("work_item", "w1"), ("thread", "t1"));
        assert_eq!(db.list_unassigned_mail_threads(10, 0).unwrap().total, 0);
        db.lock_conn().execute("DELETE FROM links", []).unwrap();
        assert_eq!(db.list_unassigned_mail_threads(10, 0).unwrap().total, 1);
    }

    #[test]
    fn paging_is_stable_and_clamped() {
        let db = fresh_db();
        for i in 1..=7i64 {
            msg(&db, "INBOX", i, &format!("t{i}"), 100 * i);
        }
        // Equal timestamps order by thread id.
        msg(&db, "INBOX", 8, "t8", 700);
        let p1 = db.list_unassigned_mail_threads(3, 0).unwrap();
        let p2 = db.list_unassigned_mail_threads(3, 3).unwrap();
        let p3 = db.list_unassigned_mail_threads(3, 6).unwrap();
        assert_eq!(ids(&p1), ["t7", "t8", "t6"]);
        assert_eq!(ids(&p2), ["t5", "t4", "t3"]);
        assert_eq!(ids(&p3), ["t2", "t1"]);
        assert!([&p1, &p2, &p3].iter().all(|p| p.total == 8));
        // Past the end: empty page, total intact. Bad limits are clamped.
        let end = db.list_unassigned_mail_threads(3, 100).unwrap();
        assert!(end.threads.is_empty() && end.total == 8);
        let clamped = db.list_unassigned_mail_threads(0, -5).unwrap();
        assert_eq!((clamped.limit, clamped.offset), (1, 0));
        assert_eq!(clamped.threads.len(), 1);
        let big = db.list_unassigned_mail_threads(10_000, 0).unwrap();
        assert_eq!(big.limit, UNASSIGNED_MAX_LIMIT);
    }

    #[test]
    fn empty_profile_is_an_empty_page() {
        let db = fresh_db();
        let p = db.list_unassigned_mail_threads(20, 0).unwrap();
        assert!(p.threads.is_empty());
        assert_eq!(p.total, 0);
    }
}
