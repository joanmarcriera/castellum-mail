// Castellum - VIP senders and the new-mail notification outbox (S59).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Rust decides *what is new*; Swift decides *how to present it*. The sync
// pass writes announceable arrivals to `notification_outbox`
// (`enqueue_notifications`); the app drains it on a short poll
// (`take_pending_notifications`, at-most-once) and applies its own policy.
// VIPs live in their own table keyed by `lower(trim(address))` so the
// contacts prune (`refresh_contact_aggregates`) can never wipe them.
// See project-docs/plans/2026-09-25-notifications-vip.md.

use crate::{Database, DbError};
use castellum_models::{PendingNotification, VipSender};
use rusqlite::{params, params_from_iter};

/// Chunk size for `IN (...)` lists (SQLite bound-variable ceiling).
const QUERY_CHUNK: usize = 500;
/// Delivered outbox rows are pruned once older than this (plan Assumption 3).
const DELIVERED_RETENTION_SECS: i64 = 7 * 86_400;
/// A message older than this when the sync pass sees it is not "new".
const ANNOUNCE_WINDOW_SECS: i64 = 86_400;

/// The stable VIP key: trimmed, ASCII-lowercased (the same key
/// `contacts.rs` uses in SQL).
pub fn vip_key(address: &str) -> String {
    address.trim().to_ascii_lowercase()
}

impl Database {
    /// Add (`vip = true`) or remove a VIP. Returns whether the stored state
    /// changed. An address without `@` is rejected with a friendly error.
    pub fn set_vip(
        &self,
        address: &str,
        display_name: Option<&str>,
        vip: bool,
        now_epoch: i64,
    ) -> Result<bool, DbError> {
        let key = vip_key(address);
        if !key.contains('@') || key.starts_with('@') || key.ends_with('@') {
            return Err(DbError(
                "That does not look like an email address, so it cannot be a VIP.".into(),
            ));
        }
        let conn = self.lock_conn();
        let changed = if vip {
            let n = conn.execute(
                "INSERT OR IGNORE INTO vip_senders (address_key, display_name, created_at)
                 VALUES (?1, ?2, ?3)",
                params![key, display_name, now_epoch],
            )?;
            if n == 0 && display_name.is_some() {
                // Already a VIP: refresh the name, but that is not a state change.
                conn.execute(
                    "UPDATE vip_senders SET display_name = ?2 WHERE address_key = ?1",
                    params![key, display_name],
                )?;
            }
            n > 0
        } else {
            conn.execute(
                "DELETE FROM vip_senders WHERE address_key = ?1",
                params![key],
            )? > 0
        };
        Ok(changed)
    }

    /// Every VIP, alphabetically by address.
    pub fn list_vips(&self) -> Result<Vec<VipSender>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT address_key, display_name, created_at FROM vip_senders ORDER BY address_key",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(VipSender {
                address: r.get(0)?,
                display_name: r.get(1)?,
                created_at: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Insert announceable messages among `message_ids` (plan Assumption 4).
    /// The caller has already dropped first-sync/UIDVALIDITY-reset mailboxes;
    /// everything that depends on state AFTER rules/mute ran is decided here:
    /// inbox role, unread, thread not muted, sent within 24 h, not snoozed,
    /// and no queued `mark_read`/`move` op for the message. `INSERT OR IGNORE`
    /// on the UNIQUE `message_id` makes this idempotent.
    pub fn enqueue_notifications(
        &self,
        account_id: &str,
        message_ids: &[String],
        now_epoch: i64,
    ) -> Result<usize, DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let mut inserted = 0usize;
        for chunk in message_ids.chunks(QUERY_CHUNK) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "INSERT OR IGNORE INTO notification_outbox (account_id, message_id, created_at)
                 SELECT m.account_id, m.id, ?1
                 FROM messages m
                 JOIN mailboxes mb ON mb.id = m.mailbox_id AND mb.role = 'inbox'
                 LEFT JOIN message_flags f ON f.message_id = m.id
                 LEFT JOIN threads t ON t.id = m.thread_id
                 WHERE m.account_id = ?2
                   AND m.id IN ({placeholders})
                   AND COALESCE(f.is_read, 0) = 0
                   AND COALESCE(t.is_muted, 0) = 0
                   AND (m.sent_at IS NULL OR m.sent_at >= ?1 - {ANNOUNCE_WINDOW_SECS})
                   AND NOT EXISTS (SELECT 1 FROM snoozed_messages s WHERE s.message_id = m.id)
                   AND NOT EXISTS (SELECT 1 FROM operations o
                                   WHERE o.account_id = m.account_id
                                     AND o.status IN ('queued','running')
                                     AND o.kind IN ('mark_read','move')
                                     AND json_extract(o.payload_json, '$.message_id') = m.id)"
            );
            let mut p: Vec<rusqlite::types::Value> =
                vec![now_epoch.into(), account_id.to_string().into()];
            p.extend(chunk.iter().map(|id| id.clone().into()));
            inserted += tx.execute(&sql, params_from_iter(p))?;
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Pop up to `limit` undelivered notices (oldest first) and mark them
    /// delivered in the same transaction: at-most-once, so a crash between
    /// take and post loses a banner rather than repeating one. Delivered
    /// rows older than 7 days are pruned here too.
    pub fn take_pending_notifications(
        &self,
        limit: i64,
        now_epoch: i64,
    ) -> Result<Vec<PendingNotification>, DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let out: Vec<PendingNotification> = {
            let mut stmt = tx.prepare(
                "SELECT o.id, o.account_id, o.message_id, m.mailbox_id, m.thread_id,
                        m.from_name, m.from_address, m.subject, m.snippet, m.sent_at,
                        EXISTS(SELECT 1 FROM vip_senders v
                               WHERE v.address_key = lower(trim(m.from_address)))
                 FROM notification_outbox o
                 JOIN messages m ON m.id = o.message_id
                 WHERE o.delivered_at IS NULL
                 ORDER BY o.id
                 LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![limit], |r| {
                Ok(PendingNotification {
                    outbox_id: r.get(0)?,
                    account_id: r.get(1)?,
                    message_id: r.get(2)?,
                    mailbox_id: r.get(3)?,
                    thread_id: r.get(4)?,
                    from_name: r.get(5)?,
                    from_address: r.get(6)?,
                    subject: r.get(7)?,
                    snippet: r.get(8)?,
                    sent_at: r.get(9)?,
                    from_vip: r.get(10)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for n in &out {
            tx.execute(
                "UPDATE notification_outbox SET delivered_at = ?2 WHERE id = ?1",
                params![n.outbox_id, now_epoch],
            )?;
        }
        tx.execute(
            "DELETE FROM notification_outbox WHERE delivered_at IS NOT NULL AND delivered_at < ?1",
            params![now_epoch - DELIVERED_RETENTION_SECS],
        )?;
        tx.commit()?;
        Ok(out)
    }

    /// Unread inbox-role messages from VIP senders (not snoozed), optionally
    /// restricted to `account_ids` (`None` = every account). Drives the
    /// "VIP unread" Dock badge.
    pub fn vip_unread_count(&self, account_ids: Option<&[String]>) -> Result<i64, DbError> {
        let conn = self.lock_conn();
        let mut sql = String::from(
            "SELECT COUNT(*)
             FROM messages m
             JOIN mailboxes mb ON mb.id = m.mailbox_id AND mb.role = 'inbox'
             LEFT JOIN message_flags f ON f.message_id = m.id
             WHERE COALESCE(f.is_read, 0) = 0
               AND EXISTS (SELECT 1 FROM vip_senders v
                           WHERE v.address_key = lower(trim(m.from_address)))
               AND NOT EXISTS (SELECT 1 FROM snoozed_messages s WHERE s.message_id = m.id)",
        );
        let mut p: Vec<String> = Vec::new();
        if let Some(ids) = account_ids {
            if ids.is_empty() {
                return Ok(0);
            }
            sql.push_str(&format!(
                " AND m.account_id IN ({})",
                vec!["?"; ids.len()].join(",")
            ));
            p.extend(ids.iter().cloned());
        }
        Ok(conn.query_row(&sql, params_from_iter(p), |r| r.get(0))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    const NOW: i64 = 1_800_000_000;

    fn fresh_db() -> Database {
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-notify-test-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        Database::open(&path).expect("open")
    }

    /// One account with an inbox (`role = 'inbox'`) and an archive mailbox.
    fn seed_base(db: &Database) {
        let conn = db.lock_conn();
        conn.execute_batch(
            "INSERT INTO accounts (id, email, provider) VALUES ('acc1', 'a@x.test', 'imap');
             INSERT INTO mailboxes (id, account_id, path, name, role)
                 VALUES ('inbox', 'acc1', 'INBOX', 'INBOX', 'inbox');
             INSERT INTO mailboxes (id, account_id, path, name, role)
                 VALUES ('arch', 'acc1', 'Archive', 'Archive', 'archive');
             INSERT INTO threads (id, account_id) VALUES ('t1', 'acc1');",
        )
        .expect("seed base");
    }

    fn add_msg(
        db: &Database,
        id: &str,
        mailbox: &str,
        from: &str,
        sent_at: Option<i64>,
        read: bool,
        thread: Option<&str>,
    ) {
        let conn = db.lock_conn();
        conn.execute(
            "INSERT INTO messages (id, account_id, mailbox_id, from_address, subject, snippet,
                                   sent_at, thread_id)
             VALUES (?1, 'acc1', ?2, ?3, 'Subject', 'Snippet', ?4, ?5)",
            params![id, mailbox, from, sent_at, thread],
        )
        .expect("insert message");
        conn.execute(
            "INSERT OR REPLACE INTO message_flags (message_id, is_read) VALUES (?1, ?2)",
            params![id, read],
        )
        .expect("insert flags");
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn outbox_rows(db: &Database) -> i64 {
        db.lock_conn()
            .query_row("SELECT COUNT(*) FROM notification_outbox", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn set_vip_is_case_and_space_insensitive() {
        let db = fresh_db();
        assert!(db
            .set_vip(" Ana@Example.COM ", Some("Ana"), true, NOW)
            .unwrap());
        let vips = db.list_vips().unwrap();
        assert_eq!(vips.len(), 1);
        assert_eq!(vips[0].address, "ana@example.com");
        assert_eq!(vips[0].display_name.as_deref(), Some("Ana"));
        assert!(!db.set_vip("ana@example.com", None, true, NOW).unwrap());
        assert!(db.set_vip("ANA@example.com", None, false, NOW).unwrap());
        assert!(db.list_vips().unwrap().is_empty());
        assert!(db.set_vip("not-an-address", None, true, NOW).is_err());
    }

    #[test]
    fn enqueue_filters_to_announceable() {
        let db = fresh_db();
        seed_base(&db);
        let recent = Some(NOW - 60);
        add_msg(&db, "ok", "inbox", "a@x.test", recent, false, None);
        add_msg(&db, "read", "inbox", "a@x.test", recent, true, None);
        add_msg(&db, "arch", "arch", "a@x.test", recent, false, None);
        add_msg(
            &db,
            "old",
            "inbox",
            "a@x.test",
            Some(NOW - 3 * 86_400),
            false,
            None,
        );
        add_msg(&db, "snoozed", "inbox", "a@x.test", recent, false, None);
        add_msg(&db, "queued", "inbox", "a@x.test", recent, false, None);
        {
            let conn = db.lock_conn();
            conn.execute(
                "INSERT INTO snoozed_messages (message_id, account_id, snoozed_until_epoch)
                 VALUES ('snoozed', 'acc1', ?1)",
                params![NOW + 3600],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO operations (account_id, kind, payload_json)
                 VALUES ('acc1', 'mark_read', '{\"message_id\":\"queued\",\"is_read\":true}')",
                [],
            )
            .unwrap();
        }
        let all = ids(&["ok", "read", "arch", "old", "snoozed", "queued"]);
        assert_eq!(db.enqueue_notifications("acc1", &all, NOW).unwrap(), 1);
        assert_eq!(outbox_rows(&db), 1);
    }

    #[test]
    fn enqueue_skips_muted_threads() {
        let db = fresh_db();
        seed_base(&db);
        add_msg(&db, "m1", "inbox", "a@x.test", Some(NOW), false, Some("t1"));
        db.lock_conn()
            .execute("UPDATE threads SET is_muted = 1 WHERE id = 't1'", [])
            .unwrap();
        assert_eq!(
            db.enqueue_notifications("acc1", &ids(&["m1"]), NOW)
                .unwrap(),
            0
        );
    }

    #[test]
    fn enqueue_is_idempotent() {
        let db = fresh_db();
        seed_base(&db);
        add_msg(&db, "m1", "inbox", "a@x.test", Some(NOW), false, None);
        assert_eq!(
            db.enqueue_notifications("acc1", &ids(&["m1"]), NOW)
                .unwrap(),
            1
        );
        assert_eq!(
            db.enqueue_notifications("acc1", &ids(&["m1"]), NOW)
                .unwrap(),
            0
        );
        assert_eq!(outbox_rows(&db), 1);
    }

    #[test]
    fn take_marks_delivered_and_joins_header_fields() {
        let db = fresh_db();
        seed_base(&db);
        add_msg(
            &db,
            "m1",
            "inbox",
            "Ana@Example.com",
            Some(NOW),
            false,
            None,
        );
        db.set_vip("ana@example.com", None, true, NOW).unwrap();
        db.enqueue_notifications("acc1", &ids(&["m1"]), NOW)
            .unwrap();
        let got = db.take_pending_notifications(50, NOW).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].message_id, "m1");
        assert_eq!(got[0].subject.as_deref(), Some("Subject"));
        assert_eq!(got[0].snippet.as_deref(), Some("Snippet"));
        assert_eq!(got[0].mailbox_id, "inbox");
        assert!(got[0].from_vip);
        assert!(db.take_pending_notifications(50, NOW).unwrap().is_empty());
    }

    #[test]
    fn take_prunes_delivered_older_than_7_days() {
        let db = fresh_db();
        seed_base(&db);
        add_msg(&db, "m1", "inbox", "a@x.test", Some(NOW), false, None);
        db.enqueue_notifications("acc1", &ids(&["m1"]), NOW)
            .unwrap();
        assert_eq!(db.take_pending_notifications(50, NOW).unwrap().len(), 1);
        // Six days later the delivered row is still kept (so the message is
        // never announced twice)...
        db.take_pending_notifications(50, NOW + 6 * 86_400).unwrap();
        assert_eq!(outbox_rows(&db), 1);
        // ...eight days later it is pruned.
        db.take_pending_notifications(50, NOW + 8 * 86_400).unwrap();
        assert_eq!(outbox_rows(&db), 0);
    }

    #[test]
    fn vip_unread_count_counts_inbox_unread_from_vips_only() {
        let db = fresh_db();
        seed_base(&db);
        db.lock_conn()
            .execute_batch(
                "INSERT INTO accounts (id, email, provider) VALUES ('acc2', 'b@x.test', 'imap');
                 INSERT INTO mailboxes (id, account_id, path, name, role)
                     VALUES ('inbox2', 'acc2', 'INBOX', 'INBOX', 'inbox');",
            )
            .unwrap();
        add_msg(&db, "v1", "inbox", "vip@x.test", Some(NOW), false, None);
        add_msg(&db, "v2", "inbox", "VIP@x.test", Some(NOW), false, None);
        add_msg(&db, "vread", "inbox", "vip@x.test", Some(NOW), true, None);
        add_msg(&db, "varch", "arch", "vip@x.test", Some(NOW), false, None);
        add_msg(
            &db,
            "other",
            "inbox",
            "other@x.test",
            Some(NOW),
            false,
            None,
        );
        db.lock_conn()
            .execute(
                "INSERT INTO messages (id, account_id, mailbox_id, from_address)
                 VALUES ('v3', 'acc2', 'inbox2', 'vip@x.test')",
                [],
            )
            .unwrap();
        db.set_vip("vip@x.test", None, true, NOW).unwrap();
        assert_eq!(db.vip_unread_count(None).unwrap(), 3);
        assert_eq!(db.vip_unread_count(Some(&ids(&["acc1"]))).unwrap(), 2);
        assert_eq!(db.vip_unread_count(Some(&[])).unwrap(), 0);
    }
}
