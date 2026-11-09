// Castellum - per-account identities/aliases (S47).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Extra From addresses a compose window's From picker can offer alongside
// an account's own `accounts.email` (which is never itself a row here —
// see migration 0026's doc comment).

use crate::{Database, DbError};
use castellum_models::{IdentityRecord, NewIdentity};
use rusqlite::{params, OptionalExtension};

fn row_to_identity(r: &rusqlite::Row<'_>) -> rusqlite::Result<IdentityRecord> {
    Ok(IdentityRecord {
        id: r.get(0)?,
        account_id: r.get(1)?,
        email: r.get(2)?,
        display_name: r.get(3)?,
        reply_to: r.get(4)?,
        bcc: r.get(5)?,
        is_default: r.get::<_, i64>(6)? != 0,
    })
}

/// A Reply-To / Bcc address: empty (or blank) clears it; otherwise exactly one
/// plain address. Guards header injection (no CR/LF) and the envelope (no
/// comma-separated lists — one address per identity, on purpose).
fn clean_optional_address(value: Option<&str>, what: &str) -> Result<Option<String>, DbError> {
    let Some(raw) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let ok = raw.contains('@')
        && !raw.starts_with('@')
        && !raw.ends_with('@')
        && !raw.chars().any(|c| {
            c.is_whitespace() || c.is_control() || matches!(c, ',' | ';' | '<' | '>' | '"')
        });
    if !ok {
        return Err(DbError(format!("invalid {what} address: {raw:?}")));
    }
    Ok(Some(raw.to_string()))
}

fn set_default_in(conn: &rusqlite::Connection, id: i64, is_default: bool) -> Result<(), DbError> {
    let account_id: Option<String> = conn
        .query_row(
            "SELECT account_id FROM account_identities WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(account_id) = account_id else {
        return Err(DbError(format!("identity {id} not found")));
    };
    if is_default {
        conn.execute(
            "UPDATE account_identities SET is_default = (id = ?2) WHERE account_id = ?1",
            params![account_id, id],
        )?;
    } else {
        conn.execute(
            "UPDATE account_identities SET is_default = 0 WHERE id = ?1",
            params![id],
        )?;
    }
    Ok(())
}

impl Database {
    /// Identities for `account_id`, oldest first (stable, predictable
    /// picker order). The account's own address is prepended by the
    /// caller — it is never stored here.
    pub fn list_identities(&self, account_id: &str) -> Result<Vec<IdentityRecord>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT id, account_id, email, display_name, reply_to, bcc, is_default
             FROM account_identities WHERE account_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![account_id], row_to_identity)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Create an identity. Refuses a blank/malformed email (no `@`) and a
    /// duplicate `(account_id, email)` pair compared case-insensitively
    /// (review of PR #206): the table's UNIQUE constraint is case-sensitive,
    /// but send.rs resolves the sending identity with a case-insensitive
    /// match, so `Work@x` next to `work@x` would make which Reply-To/Bcc
    /// applies depend on row order.
    pub fn create_identity(&self, new: &NewIdentity) -> Result<IdentityRecord, DbError> {
        let email = new.email.trim();
        if email.is_empty() || !email.contains('@') {
            return Err(DbError(format!("invalid identity email: {:?}", new.email)));
        }
        let reply_to = clean_optional_address(new.reply_to.as_deref(), "reply-to")?;
        let bcc = clean_optional_address(new.bcc.as_deref(), "bcc")?;
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let clash: Option<String> = tx
            .query_row(
                "SELECT email FROM account_identities
                 WHERE account_id = ?1 AND lower(email) = lower(?2) LIMIT 1",
                params![new.account_id, email],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(existing) = clash {
            return Err(DbError(format!(
                "create identity: this account already has the identity {existing}"
            )));
        }
        tx.execute(
            "INSERT INTO account_identities (account_id, email, display_name, reply_to, bcc)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![new.account_id, email, new.display_name, reply_to, bcc],
        )
        .map_err(|e| DbError(format!("create identity: {e}")))?;
        let id = tx.last_insert_rowid();
        if new.is_default {
            set_default_in(&tx, id, true)?;
        }
        tx.commit()?;
        conn.query_row(
            "SELECT id, account_id, email, display_name, reply_to, bcc, is_default
             FROM account_identities WHERE id = ?1",
            params![id],
            row_to_identity,
        )
        .map_err(DbError::from)
    }

    /// MM-6: change an identity's details. `None` leaves a field as it is;
    /// `Some("")` clears it. `reply_to` and `bcc` must be a single address
    /// (contain `@`, no whitespace or commas) or empty. The email itself stays
    /// immutable.
    pub fn update_identity_details(
        &self,
        id: i64,
        display_name: Option<&str>,
        reply_to: Option<&str>,
        bcc: Option<&str>,
        is_default: Option<bool>,
    ) -> Result<(), DbError> {
        let reply_to = reply_to
            .map(|v| clean_optional_address(Some(v), "reply-to"))
            .transpose()?;
        let bcc = bcc
            .map(|v| clean_optional_address(Some(v), "bcc"))
            .transpose()?;
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let conn = &tx;
        let exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM account_identities WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )?;
        if exists == 0 {
            return Err(DbError(format!("identity {id} not found")));
        }
        if let Some(name) = display_name {
            let name = name.trim();
            conn.execute(
                "UPDATE account_identities SET display_name = ?2 WHERE id = ?1",
                params![id, if name.is_empty() { None } else { Some(name) }],
            )?;
        }
        if let Some(value) = reply_to {
            conn.execute(
                "UPDATE account_identities SET reply_to = ?2 WHERE id = ?1",
                params![id, value],
            )?;
        }
        if let Some(value) = bcc {
            conn.execute(
                "UPDATE account_identities SET bcc = ?2 WHERE id = ?1",
                params![id, value],
            )?;
        }
        if let Some(flag) = is_default {
            set_default_in(conn, id, flag)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// MM-6: make `id` its account's default identity for new mail (clearing
    /// any other flagged one in the same account, in one transaction), or clear
    /// the flag so the account's own address is the default again.
    pub fn set_identity_default(&self, id: i64, is_default: bool) -> Result<(), DbError> {
        let conn = self.lock_conn();
        set_default_in(&conn, id, is_default)
    }

    /// The identity a fresh message from `account_id` should start with, or
    /// `None` for the account's own address.
    pub fn default_identity(&self, account_id: &str) -> Result<Option<IdentityRecord>, DbError> {
        Ok(self
            .list_identities(account_id)?
            .into_iter()
            .find(|i| i.is_default))
    }

    /// Rename an identity's display name. The email itself is immutable —
    /// delete and recreate to change it, keeping this a one-column update.
    pub fn update_identity_display_name(
        &self,
        id: i64,
        display_name: Option<&str>,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        let n = conn.execute(
            "UPDATE account_identities SET display_name = ?2 WHERE id = ?1",
            params![id, display_name],
        )?;
        if n == 0 {
            return Err(DbError(format!("identity {id} not found")));
        }
        Ok(())
    }

    /// Delete an identity. A draft that already denormalized this
    /// identity's email/name onto its OWN `from_email`/`from_name` columns
    /// is unaffected (migration 0026) — only the picker offering stops.
    pub fn delete_identity(&self, id: i64) -> Result<(), DbError> {
        let conn = self.lock_conn();
        let n = conn.execute("DELETE FROM account_identities WHERE id = ?1", params![id])?;
        if n == 0 {
            return Err(DbError(format!("identity {id} not found")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use castellum_models::Account;

    fn fresh_db() -> Database {
        // Unique per call, not just per clock tick: the clock is only microsecond-grained on
        // macOS, so two parallel tests could draw the same name, open the same file at once and
        // race Database::open's migrations ("duplicate column name: delimiter").
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-identities-test-{}-{}-{n}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        Database::open(&path).expect("open")
    }

    fn seed_account(db: &Database) {
        db.upsert_account(&Account {
            id: "acc1".into(),
            email: "test@local.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
    }

    fn seed_named_account(db: &Database, id: &str) {
        db.upsert_account(&Account {
            id: id.into(),
            email: format!("{id}@local.test"),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
    }

    fn new_identity(account_id: &str, email: &str, name: Option<&str>) -> NewIdentity {
        NewIdentity {
            reply_to: None,
            bcc: None,
            is_default: false,
            account_id: account_id.into(),
            email: email.into(),
            display_name: name.map(str::to_owned),
        }
    }

    #[test]
    fn create_and_list_identities_for_an_account() {
        let db = fresh_db();
        seed_account(&db);
        db.create_identity(&new_identity("acc1", "alias@x.test", Some("Alias Name")))
            .unwrap();
        db.create_identity(&new_identity("acc1", "second@x.test", None))
            .unwrap();
        let rows = db.list_identities("acc1").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].email, "alias@x.test");
        assert_eq!(rows[0].display_name.as_deref(), Some("Alias Name"));
        assert_eq!(rows[1].display_name, None);
    }

    #[test]
    fn create_identity_rejects_a_malformed_email() {
        let db = fresh_db();
        seed_account(&db);
        assert!(db
            .create_identity(&new_identity("acc1", "not-an-email", None))
            .is_err());
        assert!(db
            .create_identity(&new_identity("acc1", "  ", None))
            .is_err());
    }

    #[test]
    fn create_identity_rejects_a_duplicate_for_the_same_account() {
        let db = fresh_db();
        seed_account(&db);
        db.create_identity(&new_identity("acc1", "alias@x.test", None))
            .unwrap();
        assert!(db
            .create_identity(&new_identity("acc1", "alias@x.test", None))
            .is_err());
        // Case-insensitive, like the send-time lookup (review of PR #206).
        let err = db
            .create_identity(&new_identity("acc1", "Alias@X.test", None))
            .unwrap_err();
        assert!(err.0.contains("already has"), "{err:?}");
        assert_eq!(db.list_identities("acc1").unwrap().len(), 1);
    }

    #[test]
    fn update_display_name_then_delete() {
        let db = fresh_db();
        seed_account(&db);
        let row = db
            .create_identity(&new_identity("acc1", "alias@x.test", None))
            .unwrap();
        db.update_identity_display_name(row.id, Some("New Name"))
            .unwrap();
        assert_eq!(
            db.list_identities("acc1").unwrap()[0]
                .display_name
                .as_deref(),
            Some("New Name")
        );
        db.delete_identity(row.id).unwrap();
        assert!(db.list_identities("acc1").unwrap().is_empty());
    }

    #[test]
    fn update_and_delete_error_on_an_unknown_id() {
        let db = fresh_db();
        assert!(db.update_identity_display_name(9999, None).is_err());
        assert!(db.delete_identity(9999).is_err());
    }

    #[test]
    fn extras_are_stored_validated_and_cleared() {
        let db = fresh_db();
        seed_named_account(&db, "acc");
        let created = db
            .create_identity(&NewIdentity {
                account_id: "acc".into(),
                email: "work@x.test".into(),
                display_name: Some("Work".into()),
                reply_to: Some(" team@x.test ".into()),
                bcc: Some("archive@x.test".into()),
                is_default: false,
            })
            .unwrap();
        assert_eq!(created.reply_to.as_deref(), Some("team@x.test"));
        assert_eq!(created.bcc.as_deref(), Some("archive@x.test"));
        assert!(!created.is_default);
        for bad in [
            "a@x.test, b@x.test",
            "x\r\nBcc: evil@x.test",
            "@x",
            "nope",
            "a b@x.test",
            "<a@x.test>",
        ] {
            assert!(
                db.create_identity(&NewIdentity {
                    account_id: "acc".into(),
                    email: "other@x.test".into(),
                    display_name: None,
                    reply_to: Some(bad.into()),
                    bcc: None,
                    is_default: false,
                })
                .is_err(),
                "{bad:?} must be refused"
            );
        }
        // None leaves a field, Some("") clears it.
        db.update_identity_details(created.id, None, Some(""), None, None)
            .unwrap();
        let after = &db.list_identities("acc").unwrap()[0];
        assert_eq!(after.reply_to, None);
        assert_eq!(after.bcc.as_deref(), Some("archive@x.test"));
        assert_eq!(after.display_name.as_deref(), Some("Work"));
        assert!(db
            .update_identity_details(created.id, None, Some("bad"), None, None)
            .is_err());
        assert!(db
            .update_identity_details(9999, Some("x"), None, None, None)
            .is_err());
    }

    #[test]
    fn at_most_one_default_per_account() {
        let db = fresh_db();
        seed_named_account(&db, "acc");
        seed_named_account(&db, "other");
        let mk = |account: &str, email: &str| {
            db.create_identity(&NewIdentity {
                account_id: account.into(),
                email: email.into(),
                display_name: None,
                reply_to: None,
                bcc: None,
                is_default: false,
            })
            .unwrap()
            .id
        };
        let a = mk("acc", "a@x.test");
        let b = mk("acc", "b@x.test");
        let o = mk("other", "o@x.test");
        assert!(
            db.default_identity("acc").unwrap().is_none(),
            "the account's own address by default"
        );
        db.set_identity_default(a, true).unwrap();
        db.set_identity_default(o, true).unwrap();
        db.set_identity_default(b, true).unwrap();
        assert_eq!(db.default_identity("acc").unwrap().unwrap().id, b);
        assert_eq!(
            db.default_identity("other").unwrap().unwrap().id,
            o,
            "other accounts are untouched"
        );
        assert!(
            !db.list_identities("acc")
                .unwrap()
                .iter()
                .find(|i| i.id == a)
                .unwrap()
                .is_default
        );
        db.set_identity_default(b, false).unwrap();
        assert!(db.default_identity("acc").unwrap().is_none());
        assert!(db.set_identity_default(9999, true).is_err());
        // creating one already flagged default takes over
        let c = db
            .create_identity(&NewIdentity {
                account_id: "acc".into(),
                email: "c@x.test".into(),
                display_name: None,
                reply_to: None,
                bcc: None,
                is_default: true,
            })
            .unwrap();
        assert!(c.is_default);
        assert_eq!(db.default_identity("acc").unwrap().unwrap().id, c.id);
    }
}
