// Castellum - saved-search "smart mailbox" persistence layer (S41).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// All methods on `Database`. Smart mailbox ids are uuid-v4 strings, mirroring
// `projects.rs`. Selecting a smart mailbox runs its saved `query` through
// `search_messages_across_accounts` (S34/S39) — this file adds no second
// filter implementation, only persistence + a capped count (below the CRUD
// section).

use crate::filter_sql::{compile_group, has_header_leaf, validate_limits};
use crate::search_query;
use crate::{Database, DbError};
use castellum_models::{
    BoolOp, FilterCondition, FilterField, FilterGroup, FilterOperator, FilterValue, MessageHeader,
    SmartMailbox, SmartMailboxScope,
};
use rusqlite::{params, params_from_iter, types::Value, Connection, OptionalExtension};
use uuid::Uuid;

/// Unix epoch seconds now (fallback to 0 on a pre-1970 clock). Mirrors
/// `projects.rs::now_epoch`; each persistence file keeps its own copy rather
/// than sharing one (existing convention — see `projects.rs`/`repo.rs`).
fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Recomputation cap for a smart mailbox's cached counts (Assumption 8). A
/// capped `SELECT COUNT(*) FROM (... LIMIT cap)` bounds SQLite's work to at
/// most `cap` row-scans regardless of how broad the saved query is; past the
/// cap, `counts_capped` is set and the UI shows "`<cap>`+" instead of an
/// exact number.
pub const SMART_MAILBOX_COUNT_CAP: i64 = 2000;

/// Parameters for [`Database::upsert_smart_mailbox`]. `id: None` creates
/// (uuid-v4 generated); `id: Some(existing)` updates name/query/symbol in
/// place. Counts are NOT touched here — call
/// `Database::recompute_smart_mailbox_counts` after; the core JSON wrapper
/// does this automatically.
#[derive(Debug)]
pub struct SmartMailboxUpsert<'a> {
    pub id: Option<&'a str>,
    pub name: &'a str,
    pub query: &'a str,
    pub symbol: Option<&'a str>,
    /// MM-1: nested conditions. `Some` makes this a group mailbox (the text
    /// `query` is stored as `''`); `None` keeps a plain text-query mailbox.
    pub group: Option<&'a FilterGroup>,
    /// MM-1: account / mailbox restriction for a group mailbox.
    pub scope: Option<&'a SmartMailboxScope>,
}

/// The stored-column list shared by every SELECT, in `row_to_smart_mailbox`
/// order.
const SM_COLUMNS: &str = "id, name, query, sort_order, symbol, cached_total, cached_unread,
                counts_capped, counts_updated_at, created_at, updated_at, group_json, scope_json";

/// `group` with `scope` folded in as leading AND conditions, so a scoped
/// group mailbox runs through the ONE filter compiler/evaluator (no second
/// WHERE builder). Empty scope lists add nothing.
pub fn effective_group(group: &FilterGroup, scope: &SmartMailboxScope) -> FilterGroup {
    let mut conditions = Vec::new();
    if !scope.account_ids.is_empty() {
        conditions.push(FilterCondition {
            field: FilterField::AccountId,
            operator: FilterOperator::In,
            value: FilterValue::List(scope.account_ids.clone()),
        });
    }
    if !scope.mailbox_ids.is_empty() {
        conditions.push(FilterCondition {
            field: FilterField::MailboxId,
            operator: FilterOperator::In,
            value: FilterValue::List(scope.mailbox_ids.clone()),
        });
    }
    if conditions.is_empty() {
        return group.clone();
    }
    FilterGroup {
        op: BoolOp::And,
        conditions,
        groups: vec![group.clone()],
    }
}

/// A group mailbox must say something: an empty group compiles to "matches
/// nothing", which is a useless sidebar entry, so refuse to save it.
/// Why a smart mailbox with a Header condition cannot be an Inbox split
/// (MM-1c slice 1). Shown by the editor next to the disabled toggle.
pub const INBOX_SPLIT_HEADER_ERROR: &str = "header conditions cannot split the Inbox yet";

fn validate_group_for_save(group: &FilterGroup, scope: &SmartMailboxScope) -> Result<(), DbError> {
    fn has_condition(g: &FilterGroup) -> bool {
        !g.conditions.is_empty() || g.groups.iter().any(has_condition)
    }
    if !has_condition(group) {
        return Err(DbError("smart mailbox conditions must not be empty".into()));
    }
    // Check the tree that will actually run: scope is folded in as one
    // extra level plus up to two conditions (`effective_group`), and a group
    // that only fits without it must not be storable (its counts and list
    // would then fail on every refresh).
    // MM-1c slice 1: header conditions cannot split the Inbox. Every Inbox
    // list and count would evaluate the header subqueries for every message,
    // and a split's CASE bucket would send header-unknown mail to the next
    // tab or Other without saying so. The editor disables the toggle with
    // this reason; `compiled_splits` also skips such a stored split.
    if scope.inbox_split && has_header_leaf(group) {
        return Err(DbError(INBOX_SPLIT_HEADER_ERROR.into()));
    }
    let effective = effective_group(group, scope);
    validate_limits(&effective)?;
    // Compile once so an unsupported field/operator is rejected at save
    // time, not on every sidebar refresh.
    compile_group(&effective, &mut Vec::new())?;
    Ok(())
}

fn next_sort_order(conn: &Connection) -> Result<i64, DbError> {
    let max: Option<i64> =
        conn.query_row("SELECT MAX(sort_order) FROM smart_mailboxes", [], |r| {
            r.get(0)
        })?;
    Ok(max.map(|m| m + 1).unwrap_or(0))
}

fn fetch_smart_mailbox(conn: &Connection, id: &str) -> Result<SmartMailbox, DbError> {
    Ok(conn.query_row(
        &format!("SELECT {SM_COLUMNS} FROM smart_mailboxes WHERE id = ?1"),
        params![id],
        row_to_smart_mailbox,
    )?)
}

fn row_to_smart_mailbox(r: &rusqlite::Row<'_>) -> rusqlite::Result<SmartMailbox> {
    Ok(SmartMailbox {
        id: r.get(0)?,
        name: r.get(1)?,
        query: r.get(2)?,
        sort_order: r.get(3)?,
        symbol: r.get(4)?,
        cached_total: r.get(5)?,
        cached_unread: r.get(6)?,
        counts_capped: r.get::<_, i64>(7)? != 0,
        counts_updated_at: r.get(8)?,
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
        // A stored value that no longer parses (e.g. written by a newer
        // build) degrades to "no group" rather than failing the sidebar
        // list; such a row is repaired by editing it.
        group: r
            .get::<_, Option<String>>(11)?
            .and_then(|j| serde_json::from_str(&j).ok()),
        scope: r
            .get::<_, Option<String>>(12)?
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default(),
    })
}

impl Database {
    /// Create (`id` absent/empty — uuid-v4 generated, appended at the end of
    /// the sidebar order) or rename/re-query (`id` present) a smart mailbox.
    /// Rejects an empty name or query, and a name that collides with an
    /// existing smart mailbox (`smart_mailboxes.name` is `UNIQUE` — the
    /// `rusqlite::Error` surfaces verbatim in the returned `DbError`, the
    /// caller pattern-matches "UNIQUE constraint failed" — see Assumption 5).
    pub fn upsert_smart_mailbox(&self, u: SmartMailboxUpsert<'_>) -> Result<SmartMailbox, DbError> {
        let name = u.name.trim();
        // A group mailbox stores no text query (the group is the query).
        let query = if u.group.is_some() {
            ""
        } else {
            u.query.trim()
        };
        if name.is_empty() {
            return Err(DbError("smart mailbox name must not be empty".into()));
        }
        let (group_json, scope_json) = match u.group {
            Some(group) => {
                let scope = u.scope.cloned().unwrap_or_default();
                validate_group_for_save(group, &scope)?;
                (
                    Some(serde_json::to_string(group).map_err(|e| DbError(e.to_string()))?),
                    Some(serde_json::to_string(&scope).map_err(|e| DbError(e.to_string()))?),
                )
            }
            None => {
                if query.is_empty() {
                    return Err(DbError("smart mailbox query must not be empty".into()));
                }
                (None, None)
            }
        };
        let now = now_epoch();
        let conn = self.lock_conn();

        if let Some(id) = u.id.filter(|s| !s.is_empty()) {
            let n = conn.execute(
                "UPDATE smart_mailboxes
                 SET name = ?2, query = ?3, symbol = ?4, updated_at = ?5,
                     group_json = ?6, scope_json = ?7
                 WHERE id = ?1",
                params![id, name, query, u.symbol, now, group_json, scope_json],
            )?;
            if n == 0 {
                let sort_order = next_sort_order(&conn)?;
                conn.execute(
                    "INSERT INTO smart_mailboxes
                        (id, name, query, sort_order, symbol, cached_total, cached_unread,
                         counts_capped, counts_updated_at, created_at, updated_at,
                         group_json, scope_json)
                     VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, 0, 0, ?6, ?6, ?7, ?8)",
                    params![id, name, query, sort_order, u.symbol, now, group_json, scope_json],
                )?;
            }
            fetch_smart_mailbox(&conn, id)
        } else {
            let new_id = Uuid::new_v4().to_string();
            let sort_order = next_sort_order(&conn)?;
            conn.execute(
                "INSERT INTO smart_mailboxes
                    (id, name, query, sort_order, symbol, cached_total, cached_unread,
                     counts_capped, counts_updated_at, created_at, updated_at,
                     group_json, scope_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, 0, 0, ?6, ?6, ?7, ?8)",
                params![new_id, name, query, sort_order, u.symbol, now, group_json, scope_json],
            )?;
            fetch_smart_mailbox(&conn, &new_id)
        }
    }

    /// All smart mailboxes, sidebar order.
    pub fn list_smart_mailboxes(&self) -> Result<Vec<SmartMailbox>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {SM_COLUMNS} FROM smart_mailboxes ORDER BY sort_order, name"
        ))?;
        let rows = stmt.query_map([], row_to_smart_mailbox)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// One smart mailbox by id, or `None` if it doesn't (or no longer) exist.
    pub fn get_smart_mailbox(&self, id: &str) -> Result<Option<SmartMailbox>, DbError> {
        let conn = self.lock_conn();
        conn.query_row(
            &format!("SELECT {SM_COLUMNS} FROM smart_mailboxes WHERE id = ?1"),
            params![id],
            row_to_smart_mailbox,
        )
        .optional()
        .map_err(DbError::from)
    }

    /// Permanently delete a smart mailbox (only the saved search — never
    /// touches any mail). Errors if `id` is unknown.
    pub fn delete_smart_mailbox(&self, id: &str) -> Result<(), DbError> {
        let conn = self.lock_conn();
        let n = conn.execute("DELETE FROM smart_mailboxes WHERE id = ?1", params![id])?;
        if n == 0 {
            return Err(DbError(format!("smart mailbox {id} not found")));
        }
        Ok(())
    }

    /// Sets `sort_order` to each id's 0-based position in `ordered_ids`,
    /// atomically. Ids not present (e.g. one deleted by another window since
    /// the caller last loaded the list) are silently left untouched rather
    /// than erroring — a stale reorder must never destroy positions it
    /// doesn't know about.
    pub fn reorder_smart_mailboxes(&self, ordered_ids: &[String]) -> Result<(), DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        for (i, id) in ordered_ids.iter().enumerate() {
            tx.execute(
                "UPDATE smart_mailboxes SET sort_order = ?2 WHERE id = ?1",
                params![id, i as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Runs `query` through the same S39 compiler `search_messages_scoped`
    /// uses (`search_query::parse` + `compile`), but as two capped
    /// `COUNT(*)` queries (total, then total AND unread) instead of
    /// fetching rows — no second filter language, and cheaper than running
    /// the full search just to count its results.
    pub fn count_smart_mailbox_matches(
        &self,
        query: &str,
        cap: i64,
    ) -> Result<(i64, i64, bool), DbError> {
        let compiled = search_query::compile(&search_query::parse(query));
        if compiled.match_expr.is_none() && compiled.where_sql.is_empty() {
            // Blank, or fully degraded to nothing (e.g. a lone "-") — same
            // "nothing to search on" case search_messages_scoped short-circuits.
            return Ok((0, 0, false));
        }

        let mut from_clause = "FROM messages m".to_string();
        let mut predicates: Vec<String> = Vec::new();
        let mut params: Vec<Value> = Vec::new();
        if let Some(match_expr) = &compiled.match_expr {
            from_clause.push_str(" JOIN message_fts ON message_fts.message_id = m.id");
            predicates.push("message_fts MATCH ?".to_string());
            params.push(Value::Text(match_expr.clone()));
        }
        if !compiled.where_sql.is_empty() {
            predicates.push(compiled.where_sql.clone());
            params.extend(compiled.where_params.clone());
        }
        let predicate_sql = predicates.join(" AND ");

        let conn = self.lock_conn();

        let total_sql = format!(
            "SELECT COUNT(*) FROM (SELECT m.id {from_clause} WHERE {predicate_sql} LIMIT ?) t"
        );
        let mut total_params = params.clone();
        total_params.push(Value::Integer(cap));
        let total: i64 =
            conn.query_row(&total_sql, params_from_iter(total_params.iter()), |r| {
                r.get(0)
            })?;

        let unread_sql = format!(
            "SELECT COUNT(*) FROM (
                SELECT m.id {from_clause}
                LEFT JOIN message_flags f ON f.message_id = m.id
                WHERE {predicate_sql} AND COALESCE(f.is_read, 0) = 0
                LIMIT ?
             ) u"
        );
        let mut unread_params = params;
        unread_params.push(Value::Integer(cap));
        let unread: i64 =
            conn.query_row(&unread_sql, params_from_iter(unread_params.iter()), |r| {
                r.get(0)
            })?;

        Ok((total, unread, total >= cap || unread >= cap))
    }

    /// Capped total / unread counts for a group mailbox: the same shape as
    /// `count_smart_mailbox_matches`, over the shared filter compiler.
    ///
    /// ONE pass over the matching rows computes both counts (MM-1c review):
    /// the WHERE can now hold header subqueries, and the old second pass
    /// evaluated it again. The pass is not `LIMIT`ed: the old unread pass was
    /// already unbounded whenever fewer than `cap` matches were unread (the
    /// common case), and the counts must stay exact below the cap. Both are
    /// clamped to `cap` and `capped` is set exactly as before.
    pub fn count_group_matches(
        &self,
        group: &FilterGroup,
        scope: &SmartMailboxScope,
        cap: i64,
    ) -> Result<(i64, i64, bool), DbError> {
        let mut params: Vec<Value> = Vec::new();
        let where_sql = compile_group(&effective_group(group, scope), &mut params)?;
        let conn = self.lock_conn();
        let sql = format!(
            "SELECT COUNT(*), COALESCE(SUM(COALESCE(f.is_read, 0) = 0), 0)
             FROM messages m
             LEFT JOIN message_flags f ON f.message_id = m.id
             WHERE ({where_sql})"
        );
        let (total, unread): (i64, i64) =
            conn.query_row(&sql, params_from_iter(params.iter()), |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
        Ok((
            total.min(cap),
            unread.min(cap),
            total >= cap || unread >= cap,
        ))
    }

    /// Recomputes and stores one smart mailbox's cached counts, returning the
    /// updated row. Called right after a create/rename/query edit (the core
    /// wrapper's `upsert_smart_mailbox_json`) — never on a keystroke.
    pub fn recompute_smart_mailbox_counts(&self, id: &str) -> Result<SmartMailbox, DbError> {
        let sm = self
            .get_smart_mailbox(id)?
            .ok_or_else(|| DbError(format!("smart mailbox {id} not found")))?;
        let (total, unread, capped) = match &sm.group {
            Some(group) => self.count_group_matches(group, &sm.scope, SMART_MAILBOX_COUNT_CAP)?,
            None => self.count_smart_mailbox_matches(&sm.query, SMART_MAILBOX_COUNT_CAP)?,
        };
        let now = now_epoch();
        let conn = self.lock_conn();
        conn.execute(
            "UPDATE smart_mailboxes
             SET cached_total = ?2, cached_unread = ?3, counts_capped = ?4, counts_updated_at = ?5
             WHERE id = ?1",
            params![id, total, unread, i64::from(capped), now],
        )?;
        fetch_smart_mailbox(&conn, id)
    }

    /// Recomputes every smart mailbox's cached counts, one at a time under
    /// this crate's single serialized connection (never truly concurrent
    /// with another writer, so this is as consistent as it needs to be
    /// without a heavier transaction). Called after each sync pass (Swift
    /// `AppModel.sync`). Smart mailboxes are expected to number in the tens,
    /// not thousands, so one capped COUNT pair per row stays cheap even right
    /// after a large incoming batch of new mail.
    pub fn recompute_all_smart_mailbox_counts(&self) -> Result<Vec<SmartMailbox>, DbError> {
        let ids: Vec<String> = self
            .list_smart_mailboxes()?
            .into_iter()
            .map(|sm| sm.id)
            .collect();
        let mut updated = Vec::with_capacity(ids.len());
        for id in ids {
            // One unevaluable row (e.g. a stored group a newer build wrote)
            // must not stop every other mailbox's counts refreshing after a
            // sync; it keeps its previous cached counts.
            match self.recompute_smart_mailbox_counts(&id) {
                Ok(sm) => updated.push(sm),
                Err(e) => {
                    tracing::warn!(
                        "smart mailbox {id}: count refresh failed, keeping cached counts: {e}"
                    );
                    if let Some(sm) = self.get_smart_mailbox(&id)? {
                        updated.push(sm);
                    }
                }
            }
        }
        Ok(updated)
    }

    /// Runs a smart mailbox's saved query cross-account through
    /// `search_messages_across_accounts` — the exact same S39-compiled path
    /// "All Inboxes" search uses, never a second implementation. `Ok(None)`
    /// when `id` is unknown (deleted from another window mid-selection); the
    /// caller (Swift `AppModel`) shows an empty list with an explanation,
    /// never an error (Assumption 4).
    pub fn messages_for_smart_mailbox(
        &self,
        id: &str,
        limit: i64,
    ) -> Result<Option<Vec<MessageHeader>>, DbError> {
        let Some(sm) = self.get_smart_mailbox(id)? else {
            return Ok(None);
        };
        if let Some(group) = &sm.group {
            // Newest first, like a text-query mailbox.
            return Ok(Some(self.evaluate_filter(
                &effective_group(group, &sm.scope),
                None,
                None,
                true,
                limit,
                0,
            )?));
        }
        Ok(Some(
            self.search_messages_across_accounts(&sm.query, limit)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use castellum_models::{Account, Address, Mailbox, NewMessage};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn fresh_db() -> Database {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-smart-mailboxes-test-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        Database::open(&path).expect("open")
    }

    #[test]
    fn create_list_rename_and_delete_roundtrip() {
        let db = fresh_db();

        let created = db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: None,
                name: "Unread from Alice",
                query: "from:alice is:unread",
                symbol: None,
                group: None,
                scope: None,
            })
            .unwrap();
        assert!(!created.id.is_empty());
        assert_eq!(created.name, "Unread from Alice");
        assert_eq!(created.sort_order, 0, "first row starts at sort_order 0");
        assert_eq!(
            created.cached_total, 0,
            "not counted yet — recomputed on demand"
        );

        let second = db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: None,
                name: "Has attachments",
                query: "has:attachment",
                symbol: Some("paperclip"),
                group: None,
                scope: None,
            })
            .unwrap();
        assert_eq!(second.sort_order, 1, "next row auto-increments sort_order");

        let list = db.list_smart_mailboxes().unwrap();
        assert_eq!(
            list.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["Unread from Alice", "Has attachments"],
            "ordered by sort_order"
        );

        let renamed = db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: Some(&created.id),
                name: "Unread — Alice",
                query: "from:alice is:unread",
                symbol: None,
                group: None,
                scope: None,
            })
            .unwrap();
        assert_eq!(renamed.id, created.id, "same id, updated in place");
        assert_eq!(renamed.name, "Unread — Alice");

        assert_eq!(
            db.get_smart_mailbox(&created.id).unwrap().unwrap().name,
            "Unread — Alice"
        );
        assert!(db.get_smart_mailbox("not-an-id").unwrap().is_none());

        db.delete_smart_mailbox(&created.id).unwrap();
        assert!(db.get_smart_mailbox(&created.id).unwrap().is_none());
        assert_eq!(db.list_smart_mailboxes().unwrap().len(), 1);

        assert!(
            db.delete_smart_mailbox("not-an-id").is_err(),
            "deleting an unknown id is an error"
        );
    }

    #[test]
    fn empty_name_and_empty_query_are_rejected() {
        let db = fresh_db();
        assert!(db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: None,
                name: "  ",
                query: "from:x",
                symbol: None,
                group: None,
                scope: None
            })
            .is_err());
        assert!(db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: None,
                name: "X",
                query: "   ",
                symbol: None,
                group: None,
                scope: None
            })
            .is_err());
    }

    #[test]
    fn duplicate_name_is_rejected() {
        let db = fresh_db();
        db.upsert_smart_mailbox(SmartMailboxUpsert {
            id: None,
            name: "Dup",
            query: "a",
            symbol: None,
            group: None,
            scope: None,
        })
        .unwrap();
        let err = db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: None,
                name: "Dup",
                query: "b",
                symbol: None,
                group: None,
                scope: None,
            })
            .unwrap_err();
        assert!(err.0.contains("UNIQUE constraint failed"), "{}", err.0);
    }

    #[test]
    fn reorder_sets_sort_order_to_position_and_ignores_unknown_ids() {
        let db = fresh_db();
        let a = db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: None,
                name: "A",
                query: "a",
                symbol: None,
                group: None,
                scope: None,
            })
            .unwrap();
        let b = db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: None,
                name: "B",
                query: "b",
                symbol: None,
                group: None,
                scope: None,
            })
            .unwrap();
        let c = db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: None,
                name: "C",
                query: "c",
                symbol: None,
                group: None,
                scope: None,
            })
            .unwrap();

        db.reorder_smart_mailboxes(&[c.id.clone(), a.id.clone(), b.id.clone()])
            .unwrap();
        let list = db.list_smart_mailboxes().unwrap();
        assert_eq!(
            list.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["C", "A", "B"]
        );

        // An id no longer present (deleted concurrently by another window)
        // must not error — it's silently skipped.
        db.reorder_smart_mailboxes(&["not-an-id".into(), b.id.clone(), a.id.clone(), c.id.clone()])
            .unwrap();
        let list = db.list_smart_mailboxes().unwrap();
        assert_eq!(
            list.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["B", "A", "C"]
        );
    }

    fn seed_account_and_mailbox(db: &Database) {
        db.upsert_account(&Account {
            id: "acc1".into(),
            email: "me@local.test".into(),
            display_name: None,
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

    fn seed_msg(db: &Database, uid: i64, from: &str, subject: &str) -> String {
        let id = format!("acc1:INBOX:{uid}");
        db.upsert_message(&NewMessage {
            id: id.clone(),
            account_id: "acc1".into(),
            mailbox_id: "acc1:INBOX".into(),
            provider_uid: Some(uid),
            rfc822_message_id: Some(format!("<{uid}@local.test>")),
            subject: Some(subject.into()),
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

    #[test]
    fn recompute_counts_reuses_the_s39_compiler_and_caches_total_and_unread() {
        let db = fresh_db();
        seed_account_and_mailbox(&db);
        let m1 = seed_msg(&db, 1, "alice@x.test", "Budget");
        let m2 = seed_msg(&db, 2, "alice@x.test", "Report");
        seed_msg(&db, 3, "bob@x.test", "Not alice"); // must not match from:alice

        db.mark_read_auto(&m1, true).unwrap(); // m1 read, m2 unread (local-only flip, no write-policy gate)
        let _ = m2;

        let sm = db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: None,
                name: "Alice",
                query: "from:alice",
                symbol: None,
                group: None,
                scope: None,
            })
            .unwrap();
        assert_eq!(sm.cached_total, 0, "not counted at create time");

        let recomputed = db.recompute_smart_mailbox_counts(&sm.id).unwrap();
        assert_eq!(
            recomputed.cached_total, 2,
            "matches both alice messages, not bob's"
        );
        assert_eq!(recomputed.cached_unread, 1, "only m2 is unread");
        assert!(!recomputed.counts_capped);
        assert!(recomputed.counts_updated_at > 0);

        // Stored, not just returned.
        assert_eq!(
            db.get_smart_mailbox(&sm.id).unwrap().unwrap().cached_total,
            2
        );
    }

    #[test]
    fn recompute_all_updates_every_smart_mailbox() {
        let db = fresh_db();
        seed_account_and_mailbox(&db);
        seed_msg(&db, 1, "alice@x.test", "Budget");
        seed_msg(&db, 2, "bob@x.test", "Report");

        db.upsert_smart_mailbox(SmartMailboxUpsert {
            id: None,
            name: "Alice",
            query: "from:alice",
            symbol: None,
            group: None,
            scope: None,
        })
        .unwrap();
        db.upsert_smart_mailbox(SmartMailboxUpsert {
            id: None,
            name: "Bob",
            query: "from:bob",
            symbol: None,
            group: None,
            scope: None,
        })
        .unwrap();

        let all = db.recompute_all_smart_mailbox_counts().unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().all(|sm| sm.cached_total == 1));
    }

    #[test]
    fn recompute_all_keeps_going_and_keeps_cached_counts_when_one_row_fails() {
        let db = fresh_db();
        seed_account_and_mailbox(&db);
        seed_msg(&db, 1, "alice@x.test", "Budget");
        let mk = |name: &str, q: &str| {
            db.upsert_smart_mailbox(SmartMailboxUpsert {
                id: None,
                name,
                query: q,
                symbol: None,
                group: None,
                scope: None,
            })
            .unwrap()
        };
        let bad = mk("Bad", "from:alice");
        let good = mk("Good", "from:alice");
        // A stored group deeper than MAX_GROUP_DEPTH (e.g. written by a newer
        // build) cannot be evaluated.
        let mut deep = String::from(r#"{"op":"and","conditions":[],"groups":[]}"#);
        for _ in 0..(crate::filter_sql::MAX_GROUP_DEPTH + 2) {
            deep = format!(r#"{{"op":"and","conditions":[],"groups":[{deep}]}}"#);
        }
        db.lock_conn()
            .execute(
                "UPDATE smart_mailboxes SET group_json = ?2, cached_total = 7 WHERE id = ?1",
                params![bad.id, deep],
            )
            .unwrap();
        assert!(db.recompute_smart_mailbox_counts(&bad.id).is_err());
        let all = db.recompute_all_smart_mailbox_counts().unwrap();
        assert_eq!(all.len(), 2, "the failing row does not drop the others");
        let bad_after = all.iter().find(|m| m.id == bad.id).unwrap();
        assert_eq!(bad_after.cached_total, 7, "failing row keeps cached counts");
        assert_eq!(
            all.iter().find(|m| m.id == good.id).unwrap().cached_total,
            1
        );
    }

    #[test]
    fn count_is_capped_and_marks_itself_capped() {
        let db = fresh_db();
        seed_account_and_mailbox(&db);
        for uid in 1..=5 {
            seed_msg(&db, uid, "alice@x.test", "Hi");
        }
        let sm = db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: None,
                name: "Alice",
                query: "from:alice",
                symbol: None,
                group: None,
                scope: None,
            })
            .unwrap();

        let (total, unread, capped) = db.count_smart_mailbox_matches("from:alice", 2).unwrap();
        assert_eq!(total, 2, "capped at 2 even though 5 match");
        assert_eq!(unread, 2);
        assert!(capped);
        let _ = sm;
    }

    #[test]
    fn a_blank_or_fully_degraded_query_counts_as_zero_not_an_error() {
        let db = fresh_db();
        seed_account_and_mailbox(&db);
        seed_msg(&db, 1, "alice@x.test", "Hi");
        // "-" alone negates nothing and produces no filter/term at all
        // (search_query::parse, tested in search_query.rs itself).
        let (total, unread, capped) = db.count_smart_mailbox_matches("-", 2000).unwrap();
        assert_eq!((total, unread, capped), (0, 0, false));
    }

    #[test]
    fn messages_for_smart_mailbox_runs_the_saved_query_cross_account() {
        let db = fresh_db();
        seed_account_and_mailbox(&db);
        seed_msg(&db, 1, "alice@x.test", "Budget");
        seed_msg(&db, 2, "bob@x.test", "Report");

        let sm = db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: None,
                name: "Alice",
                query: "from:alice",
                symbol: None,
                group: None,
                scope: None,
            })
            .unwrap();

        let rows = db.messages_for_smart_mailbox(&sm.id, 50).unwrap().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].from_address.as_deref(), Some("alice@x.test"));
    }

    #[test]
    fn messages_for_smart_mailbox_returns_none_for_an_unknown_id() {
        let db = fresh_db();
        assert!(db
            .messages_for_smart_mailbox("not-an-id", 50)
            .unwrap()
            .is_none());
    }

    // ---- MM-1: group mailboxes ----

    fn cond(field: FilterField, op: FilterOperator, value: FilterValue) -> FilterCondition {
        FilterCondition {
            field,
            operator: op,
            value,
        }
    }

    fn seed(db: &Database, id: &str, mailbox: &str, subject: &str, from: &str, read: bool) {
        let uid: i64 = id
            .rsplit(':')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);
        db.upsert_message(&NewMessage {
            id: id.into(),
            account_id: "a1".into(),
            mailbox_id: mailbox.into(),
            provider_uid: Some(uid),
            rfc822_message_id: Some(format!("<{id}@x.test>")),
            subject: Some(subject.into()),
            from_name: None,
            from_address: Some(from.into()),
            sent_at: Some(1_000 + uid),
            snippet: None,
            size_bytes: None,
            has_attachments: false,
            attachments: None,
            is_read: read,
            is_starred: false,
            addresses: vec![Address {
                kind: "from".into(),
                name: None,
                address: from.into(),
            }],
            text_body: None,
            html_body: None,
            sanitized_html: None,
            has_remote_content: false,
            references: vec![],
        })
        .unwrap();
    }

    fn seeded_two_mailboxes() -> (Database, std::path::PathBuf) {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-smart-groups-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        let db = Database::open(&path).unwrap();
        db.upsert_account(&Account {
            id: "a1".into(),
            email: "me@x.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        for (id, path_name) in [("a1:INBOX", "INBOX"), ("a1:Work", "Work")] {
            db.upsert_mailbox(&Mailbox {
                id: id.into(),
                account_id: "a1".into(),
                path: path_name.into(),
                name: path_name.into(),
                role: None,
                delimiter: Some("/".into()),
                subscribed: true,
                selectable: true,
                sync_by_default: true,
            })
            .unwrap();
        }
        seed(
            &db,
            "a1:INBOX:1",
            "a1:INBOX",
            "Invoice A",
            "billing@x.test",
            false,
        );
        seed(&db, "a1:INBOX:2", "a1:INBOX", "Hello", "alice@x.test", true);
        seed(
            &db,
            "a1:Work:3",
            "a1:Work",
            "Invoice B",
            "billing@x.test",
            false,
        );
        seed(&db, "a1:Work:4", "a1:Work", "Sprint", "bob@x.test", false);
        (db, path)
    }

    fn nested_group() -> FilterGroup {
        // all( unread, none( from alice ) ) -> Invoice A, Invoice B, Sprint
        FilterGroup {
            op: BoolOp::And,
            conditions: vec![cond(
                FilterField::IsRead,
                FilterOperator::Is,
                FilterValue::Bool(false),
            )],
            groups: vec![FilterGroup {
                op: BoolOp::None,
                conditions: vec![cond(
                    FilterField::From,
                    FilterOperator::Contains,
                    FilterValue::Text("alice".into()),
                )],
                groups: vec![],
            }],
        }
    }

    #[test]
    fn group_mailbox_persists_across_reopen_and_lists_messages_and_counts() {
        let (db, path) = seeded_two_mailboxes();
        let g = nested_group();
        let created = db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: None,
                name: "Unread not Alice",
                query: "ignored for group mailboxes",
                symbol: None,
                group: Some(&g),
                scope: None,
            })
            .unwrap();
        assert_eq!(created.query, "", "a group mailbox stores no text query");
        assert_eq!(created.group.as_ref(), Some(&g));
        let counted = db.recompute_smart_mailbox_counts(&created.id).unwrap();
        assert_eq!((counted.cached_total, counted.cached_unread), (3, 3));
        let listed = db
            .messages_for_smart_mailbox(&created.id, 50)
            .unwrap()
            .unwrap();
        let mut subjects: Vec<_> = listed.iter().filter_map(|h| h.subject.clone()).collect();
        subjects.sort();
        assert_eq!(subjects, vec!["Invoice A", "Invoice B", "Sprint"]);

        // "Synced across restart": reopen the same file, get the same group.
        drop(db);
        let reopened = Database::open(&path).unwrap();
        let again = reopened.get_smart_mailbox(&created.id).unwrap().unwrap();
        assert_eq!(again.group.as_ref(), Some(&g));
        assert_eq!(again.cached_total, 3);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn scope_restricts_a_group_mailbox_to_chosen_mailboxes() {
        let (db, path) = seeded_two_mailboxes();
        let g = nested_group();
        let scope = SmartMailboxScope {
            account_ids: vec![],
            mailbox_ids: vec!["a1:Work".into()],
            ..Default::default()
        };
        let sm = db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: None,
                name: "Work unread",
                query: "",
                symbol: None,
                group: Some(&g),
                scope: Some(&scope),
            })
            .unwrap();
        assert_eq!(sm.scope, scope);
        let counted = db.recompute_smart_mailbox_counts(&sm.id).unwrap();
        assert_eq!(counted.cached_total, 2, "Invoice B + Sprint only");
        let other = SmartMailboxScope {
            account_ids: vec!["nobody".into()],
            mailbox_ids: vec![],
            ..Default::default()
        };
        assert_eq!(db.count_group_matches(&g, &other, 100).unwrap().0, 0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn editing_a_group_mailbox_and_switching_it_back_to_a_query_works() {
        let (db, path) = seeded_two_mailboxes();
        let g = nested_group();
        let sm = db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: None,
                name: "X",
                query: "",
                symbol: None,
                group: Some(&g),
                scope: None,
            })
            .unwrap();
        let back = db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: Some(&sm.id),
                name: "X",
                query: "from:alice",
                symbol: None,
                group: None,
                scope: None,
            })
            .unwrap();
        assert!(back.group.is_none());
        assert_eq!(back.query, "from:alice");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn empty_or_too_deep_groups_are_refused_at_save() {
        let (db, path) = seeded_two_mailboxes();
        let empty = FilterGroup {
            op: BoolOp::And,
            conditions: vec![],
            groups: vec![FilterGroup {
                op: BoolOp::Or,
                conditions: vec![],
                groups: vec![],
            }],
        };
        assert!(db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: None,
                name: "E",
                query: "",
                symbol: None,
                group: Some(&empty),
                scope: None,
            })
            .is_err());
        let mut deep = FilterGroup {
            op: BoolOp::And,
            conditions: vec![cond(
                FilterField::IsRead,
                FilterOperator::Is,
                FilterValue::Bool(true),
            )],
            groups: vec![],
        };
        for _ in 0..crate::filter_sql::MAX_GROUP_DEPTH {
            deep = FilterGroup {
                op: BoolOp::Or,
                conditions: vec![],
                groups: vec![deep],
            };
        }
        assert!(db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: None,
                name: "D",
                query: "",
                symbol: None,
                group: Some(&deep),
                scope: None,
            })
            .is_err());
        // an unsupported field is rejected at save, not on every refresh
        let gist = FilterGroup {
            op: BoolOp::And,
            conditions: vec![cond(
                FilterField::GistTopic,
                FilterOperator::In,
                FilterValue::List(vec![]),
            )],
            groups: vec![],
        };
        assert!(db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: None,
                name: "G",
                query: "",
                symbol: None,
                group: Some(&gist),
                scope: None,
            })
            .is_err());
        assert!(db.list_smart_mailboxes().unwrap().is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn legacy_text_mailboxes_are_unaffected_by_the_new_columns() {
        let (db, path) = seeded_two_mailboxes();
        let sm = db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: None,
                name: "Billing",
                query: "from:billing",
                symbol: None,
                group: None,
                scope: None,
            })
            .unwrap();
        assert!(sm.group.is_none());
        assert_eq!(sm.scope, SmartMailboxScope::default());
        let counted = db.recompute_smart_mailbox_counts(&sm.id).unwrap();
        assert_eq!(counted.cached_total, 2);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_group_that_only_fits_without_its_scope_is_refused_at_save() {
        let (db, path) = seeded_two_mailboxes();
        // depth exactly MAX_GROUP_DEPTH: fine bare, too deep once scope wraps it
        let mut deep = FilterGroup {
            op: BoolOp::And,
            conditions: vec![cond(
                FilterField::IsRead,
                FilterOperator::Is,
                FilterValue::Bool(true),
            )],
            groups: vec![],
        };
        for _ in 1..crate::filter_sql::MAX_GROUP_DEPTH {
            deep = FilterGroup {
                op: BoolOp::Or,
                conditions: vec![],
                groups: vec![deep],
            };
        }
        assert!(db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: None,
                name: "bare",
                query: "",
                symbol: None,
                group: Some(&deep),
                scope: None
            })
            .is_ok());
        let scope = SmartMailboxScope {
            account_ids: vec!["a1".into()],
            mailbox_ids: vec![],
            ..Default::default()
        };
        let err = db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: None,
                name: "scoped",
                query: "",
                symbol: None,
                group: Some(&deep),
                scope: Some(&scope),
            })
            .unwrap_err();
        assert!(err.0.contains("nested"), "{}", err.0);
        assert_eq!(
            db.list_smart_mailboxes().unwrap().len(),
            1,
            "nothing stored for the refused one"
        );
        // and one unevaluable stored row cannot block refreshing the others
        db.lock_conn()
            .execute("UPDATE smart_mailboxes SET group_json = ?1 WHERE name = 'bare'",
                rusqlite::params![r#"{"op":"and","conditions":[{"field":"gist_topic","operator":"in","value":["x"]}],"groups":[]}"#])
            .unwrap();
        let ok = db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: None,
                name: "fine",
                query: "from:alice",
                symbol: None,
                group: None,
                scope: None,
            })
            .unwrap();
        let refreshed = db.recompute_all_smart_mailbox_counts().unwrap();
        assert_eq!(refreshed.len(), 2);
        assert!(refreshed.iter().any(|m| m.id == ok.id));
        let _ = std::fs::remove_file(path);
    }

    // ---- MM-2: every new search qualifier executes as valid SQL ----

    #[allow(clippy::too_many_arguments)]
    fn seed_rich(
        db: &Database,
        id: &str,
        mailbox: &str,
        subject: &str,
        body: &str,
        sent_at: i64,
        size: i64,
        attachment: Option<&str>,
    ) {
        let uid: i64 = id
            .rsplit(':')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);
        db.upsert_message(&NewMessage {
            id: id.into(),
            account_id: "a1".into(),
            mailbox_id: mailbox.into(),
            provider_uid: Some(uid),
            rfc822_message_id: Some(format!("<{id}@x.test>")),
            subject: Some(subject.into()),
            from_name: None,
            from_address: Some("alice@x.test".into()),
            sent_at: Some(sent_at),
            snippet: None,
            size_bytes: Some(size),
            has_attachments: attachment.is_some(),
            attachments: attachment.map(|name| {
                vec![castellum_models::AttachmentDescriptor {
                    ordinal: 0,
                    filename: Some(name.into()),
                    content_type: Some("application/pdf".into()),
                    size_bytes: Some(10),
                    section: "2".into(),
                    content_id: None,
                    is_inline: false,
                    transfer_encoding: None,
                }]
            }),
            is_read: false,
            is_starred: false,
            addresses: vec![
                Address {
                    kind: "from".into(),
                    name: None,
                    address: "alice@x.test".into(),
                },
                Address {
                    kind: "bcc".into(),
                    name: None,
                    address: "boss@x.test".into(),
                },
            ],
            text_body: Some(body.into()),
            html_body: None,
            sanitized_html: None,
            has_remote_content: false,
            references: vec![],
        })
        .unwrap();
    }

    #[test]
    fn mm2_qualifiers_run_against_a_real_database() {
        let db = fresh_db();
        db.upsert_account(&Account {
            id: "a1".into(),
            email: "me@x.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        for (id, name) in [("a1:INBOX", "INBOX"), ("a1:Work", "Work")] {
            db.upsert_mailbox(&Mailbox {
                id: id.into(),
                account_id: "a1".into(),
                path: name.into(),
                name: name.into(),
                role: None,
                delimiter: Some("/".into()),
                subscribed: true,
                selectable: true,
                sync_by_default: true,
            })
            .unwrap();
        }
        let day = 86_400;
        let now = now_epoch();
        seed_rich(
            &db,
            "a1:INBOX:11",
            "a1:INBOX",
            "Budget",
            "the quarterly numbers",
            now - 2 * day,
            5_000,
            None,
        );
        seed_rich(
            &db,
            "a1:Work:12",
            "a1:Work",
            "quarterly",
            "nothing relevant",
            1_773_576_000, /* 2026-03-15 */
            3_000_000,
            Some("invoice.pdf"),
        );
        let find = |q: &str| -> Vec<String> {
            let mut v: Vec<String> = db
                .search_messages_across_accounts(q, 50)
                .unwrap_or_else(|e| panic!("{q}: {e}"))
                .into_iter()
                .filter_map(|h| h.subject)
                .collect();
            v.sort();
            v
        };
        // body: hits only the body column; subject: only the subject.
        assert_eq!(find("body:quarterly"), vec!["Budget"]);
        assert_eq!(find("subject:quarterly"), vec!["quarterly"]);
        assert_eq!(find("bcc:boss"), vec!["Budget", "quarterly"]);
        // exact address vs contains: "alic" matches alice@x.test, "=alic" does not
        assert_eq!(find("from:alic"), vec!["Budget", "quarterly"]);
        assert!(find("from:=alic").is_empty());
        assert_eq!(find("from:=ALICE@x.test"), vec!["Budget", "quarterly"]);
        assert_eq!(find("attachment:invoice"), vec!["quarterly"]);
        assert_eq!(find("folder:work"), vec!["quarterly"]);
        assert_eq!(
            find("folder:INBOX -folder:work body:quarterly"),
            vec!["Budget"]
        );
        assert_eq!(find("size:>1mb"), vec!["quarterly"]);
        assert_eq!(
            find("size:<10k flag:unanswered subject:Budget"),
            vec!["Budget"]
        );
        assert_eq!(find("date:2026-03"), vec!["quarterly"]);
        assert_eq!(find("date:2026-03-15"), vec!["quarterly"]);
        assert_eq!(
            find("date:2026-04.."),
            Vec::<String>::new()
                .into_iter()
                .chain(find("after:2026-04-01"))
                .collect::<Vec<_>>()
        );
        assert_eq!(find("within:7d"), vec!["Budget"]);
        // exact path vs forgiving folder name, and negation keeps everything else
        assert_eq!(find("path:Work"), vec!["quarterly"]);
        assert_eq!(
            find("path:work"),
            Vec::<String>::new(),
            "path: is case-sensitive"
        );
        assert_eq!(find("-date:2026-03 flag:unanswered"), vec!["Budget"]);
        assert_eq!(find("-size:>1mb flag:unanswered"), vec!["Budget"]);
        assert!(find("flag:answered").is_empty());
        assert!(find("is:draft").is_empty());
        // Hostile values still execute (and match nothing) rather than error.
        for q in [
            "body:'\";DROP TABLE messages;--",
            "attachment:%_\\",
            "folder:\"",
            "size:>99999999999999999999",
            "date:..",
            "within:7",
        ] {
            let _ = db.search_messages_across_accounts(q, 5).unwrap();
        }
        assert_eq!(find("subject:Budget"), vec!["Budget"], "tables intact");
    }

    // ---- MM-1b: FilterField::Tag over real message_keywords rows ----

    /// Three messages: m1 tagged `Work`, m2 tagged `Later` (display name
    /// "To do"), m3 untagged. Keyword keys are lower-cased, as on the write path.
    fn seed_tagged(db: &Database) {
        seed_account_and_mailbox(db);
        for (uid, subject) in [(1, "One"), (2, "Two"), (3, "Three")] {
            seed_msg(db, uid, "a@x.test", subject);
        }
        let conn = db.lock_conn();
        for (id, key, kw) in [
            ("acc1:INBOX:1", "work", "Work"),
            ("acc1:INBOX:2", "later", "Later"),
        ] {
            conn.execute(
                "INSERT INTO message_keywords (message_id, keyword_key, keyword) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, key, kw],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO tag_definitions (keyword_key, keyword, display_name, created_at, updated_at) \
             VALUES ('later', 'Later', 'To do', 0, 0)",
            [],
        )
        .unwrap();
    }

    fn tag_group(op: BoolOp, operator: FilterOperator, value: FilterValue) -> FilterGroup {
        FilterGroup {
            op,
            conditions: vec![cond(FilterField::Tag, operator, value)],
            groups: vec![],
        }
    }

    fn tag_count(db: &Database, g: &FilterGroup) -> i64 {
        db.count_group_matches(g, &SmartMailboxScope::default(), 100)
            .unwrap()
            .0
    }

    #[test]
    fn tag_condition_matches_no_match_and_is_case_insensitive() {
        let db = fresh_db();
        seed_tagged(&db);
        let is = |v: &str| {
            tag_group(
                BoolOp::And,
                FilterOperator::Equals,
                FilterValue::Text(v.into()),
            )
        };
        assert_eq!(tag_count(&db, &is("work")), 1, "exact key");
        assert_eq!(tag_count(&db, &is("WoRk")), 1, "case-insensitive");
        assert_eq!(tag_count(&db, &is("nothing")), 0, "no such tag");
        assert_eq!(tag_count(&db, &is("to DO")), 1, "display name, any case");
    }

    #[test]
    fn tag_condition_negated_via_a_none_group() {
        let db = fresh_db();
        seed_tagged(&db);
        let g = tag_group(
            BoolOp::None,
            FilterOperator::Equals,
            FilterValue::Text("Work".into()),
        );
        assert_eq!(tag_count(&db, &g), 2, "is-not Work: Later + untagged");
    }

    #[test]
    fn tag_has_any_and_has_no_tag() {
        let db = fresh_db();
        seed_tagged(&db);
        let any = tag_group(BoolOp::And, FilterOperator::Is, FilterValue::Bool(true));
        let none = tag_group(BoolOp::And, FilterOperator::Is, FilterValue::Bool(false));
        assert_eq!(tag_count(&db, &any), 2);
        assert_eq!(tag_count(&db, &none), 1);
    }

    /// A message whose only keywords are hidden protocol ones shows no tag
    /// in the UI, so `has any tag` / `no tag` must treat it as untagged
    /// (MM1c), including through the cached smart-mailbox recount.
    #[test]
    fn hidden_only_keywords_do_not_count_as_a_tag() {
        let db = fresh_db();
        seed_tagged(&db);
        seed_msg(&db, 4, "a@x.test", "Four");
        {
            let conn = db.lock_conn();
            for (key, kw) in [
                ("$forwarded", "$Forwarded"),
                ("nonjunk", "NonJunk"),
                ("$mailflagbit1", "$MailFlagBit1"),
                ("$muted", "$Muted"),
            ] {
                conn.execute(
                    "INSERT INTO message_keywords (message_id, keyword_key, keyword) VALUES ('acc1:INBOX:4', ?1, ?2)",
                    rusqlite::params![key, kw],
                )
                .unwrap();
            }
        }
        let any = tag_group(BoolOp::And, FilterOperator::Is, FilterValue::Bool(true));
        let none = tag_group(BoolOp::And, FilterOperator::Is, FilterValue::Bool(false));
        assert_eq!(tag_count(&db, &any), 2, "hidden-only message is not tagged");
        assert_eq!(tag_count(&db, &none), 2, "untagged + hidden-only");
        let sm = db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: None,
                name: "No tag",
                query: "",
                symbol: None,
                group: Some(&none),
                scope: None,
            })
            .unwrap();
        assert_eq!(
            db.recompute_smart_mailbox_counts(&sm.id)
                .unwrap()
                .cached_total,
            2
        );
        // Equals on a hidden keyword still matches (tag: search qualifier).
        let fwd = tag_group(
            BoolOp::And,
            FilterOperator::Equals,
            FilterValue::Text("$Forwarded".into()),
        );
        assert_eq!(tag_count(&db, &fwd), 1);
    }

    #[test]
    fn tag_value_is_bound_never_interpolated() {
        let db = fresh_db();
        seed_tagged(&db);
        let g = tag_group(
            BoolOp::And,
            FilterOperator::Equals,
            FilterValue::Text("x') OR 1=1 --".into()),
        );
        assert_eq!(tag_count(&db, &g), 0);
        let mut params = Vec::new();
        let sql = compile_group(&g, &mut params).unwrap();
        assert!(!sql.contains("OR 1=1"), "{sql}");
    }

    #[test]
    fn a_smart_mailbox_on_a_tag_condition_recounts_through_the_normal_path() {
        let db = fresh_db();
        seed_tagged(&db);
        let g = tag_group(
            BoolOp::And,
            FilterOperator::Equals,
            FilterValue::Text("work".into()),
        );
        let sm = db
            .upsert_smart_mailbox(SmartMailboxUpsert {
                id: None,
                name: "Work tag",
                query: "",
                symbol: None,
                group: Some(&g),
                scope: None,
            })
            .unwrap();
        let counted = db.recompute_smart_mailbox_counts(&sm.id).unwrap();
        assert_eq!(counted.cached_total, 1);
    }

    // --- MM-1c header conditions ---------------------------------------------

    fn sh(name: &str, ordinal: u32, value: &str) -> castellum_models::StoredHeader {
        castellum_models::StoredHeader {
            name: name.into(),
            ordinal,
            value: value.into(),
        }
    }

    /// m1: List-Id dev + two X-Mailer; m2: List-Id news; m3: captured, no
    /// headers kept; m4: never captured (unknown).
    fn seed_headers(db: &Database) {
        seed_account_and_mailbox(db);
        for uid in 1..=4 {
            seed_msg(db, uid, "a@x.test", &format!("M{uid}"));
        }
        use castellum_models::HeaderCaptureSource::Body;
        db.replace_message_headers(
            "acc1:INBOX:1",
            &[
                sh("list-id", 0, "Dev list <dev.example.org>"),
                sh("x-mailer", 0, "Foo 1.0"),
                sh("x-mailer", 1, "Bar 2.0"),
            ],
            Body,
            false,
        )
        .unwrap();
        db.replace_message_headers(
            "acc1:INBOX:2",
            &[sh("list-id", 0, "News <news.example.org>")],
            Body,
            false,
        )
        .unwrap();
        db.replace_message_headers("acc1:INBOX:3", &[], Body, false)
            .unwrap();
    }

    fn hdr_group(
        op: BoolOp,
        name: &str,
        operator: FilterOperator,
        value: FilterValue,
    ) -> FilterGroup {
        FilterGroup {
            op,
            conditions: vec![cond(FilterField::Header(name.into()), operator, value)],
            groups: vec![],
        }
    }

    fn text(v: &str) -> FilterValue {
        FilterValue::Text(v.into())
    }

    #[test]
    fn header_contains_equals_starts_with_and_any_occurrence() {
        let db = fresh_db();
        seed_headers(&db);
        let n = |g: &FilterGroup| tag_count(&db, g);
        assert_eq!(
            n(&hdr_group(
                BoolOp::And,
                "List-Id",
                FilterOperator::Contains,
                text("EXAMPLE.org")
            )),
            2
        );
        assert_eq!(
            n(&hdr_group(
                BoolOp::And,
                "list-id",
                FilterOperator::Contains,
                text("dev.")
            )),
            1
        );
        assert_eq!(
            n(&hdr_group(
                BoolOp::And,
                "List-ID:",
                FilterOperator::StartsWith,
                text("news")
            )),
            1
        );
        assert_eq!(
            n(&hdr_group(
                BoolOp::And,
                "List-Id",
                FilterOperator::Equals,
                text("dev")
            )),
            0
        );
        // Any occurrence: the SECOND X-Mailer matches.
        assert_eq!(
            n(&hdr_group(
                BoolOp::And,
                "X-Mailer",
                FilterOperator::Contains,
                text("bar")
            )),
            1
        );
        // Unknown header name matches nothing, no error.
        assert_eq!(
            n(&hdr_group(
                BoolOp::And,
                "X-Nope",
                FilterOperator::Contains,
                text("a")
            )),
            0
        );
        // Wildcards in the user's text are literal.
        assert_eq!(
            n(&hdr_group(
                BoolOp::And,
                "List-Id",
                FilterOperator::Contains,
                text("%")
            )),
            0
        );
    }

    #[test]
    fn header_present_absent_and_uncaptured_is_not_absent() {
        let db = fresh_db();
        seed_headers(&db);
        let present = hdr_group(
            BoolOp::And,
            "List-Id",
            FilterOperator::Is,
            FilterValue::Bool(true),
        );
        let absent = hdr_group(
            BoolOp::And,
            "List-Id",
            FilterOperator::Is,
            FilterValue::Bool(false),
        );
        assert_eq!(tag_count(&db, &present), 2);
        // Only m3 (captured, no List-Id): m4 was never captured so it is unknown.
        assert_eq!(tag_count(&db, &absent), 1);
        // Absent for a name nobody has: every CAPTURED message (m1..m3).
        let none_have = hdr_group(
            BoolOp::And,
            "X-Nothing",
            FilterOperator::Is,
            FilterValue::Bool(false),
        );
        assert_eq!(tag_count(&db, &none_have), 3);
    }

    #[test]
    fn header_inside_none_group_and_combined_with_scope_and_other_fields() {
        let db = fresh_db();
        seed_headers(&db);
        // "none of: List-Id contains dev": m2 and m3 (complete captures). m4 was
        // never captured, so it is unknown and does not match (fail closed).
        let none = hdr_group(
            BoolOp::None,
            "List-Id",
            FilterOperator::Contains,
            text("dev"),
        );
        assert_eq!(tag_count(&db, &none), 2);
        let mut both = hdr_group(
            BoolOp::And,
            "List-Id",
            FilterOperator::Is,
            FilterValue::Bool(true),
        );
        both.conditions.push(cond(
            FilterField::Subject,
            FilterOperator::Equals,
            text("M2"),
        ));
        assert_eq!(tag_count(&db, &both), 1);
        let scope = SmartMailboxScope {
            account_ids: vec!["other".into()],
            ..SmartMailboxScope::default()
        };
        let g = hdr_group(
            BoolOp::And,
            "List-Id",
            FilterOperator::Is,
            FilterValue::Bool(true),
        );
        assert_eq!(db.count_group_matches(&g, &scope, 100).unwrap().0, 0);
    }

    #[test]
    fn header_upsert_rejects_bad_names_operators_and_empty_values() {
        let db = fresh_db();
        seed_headers(&db);
        let bad = [
            hdr_group(BoolOp::And, "From", FilterOperator::Contains, text("a")),
            hdr_group(BoolOp::And, "Received", FilterOperator::Contains, text("a")),
            hdr_group(
                BoolOp::And,
                "Authorization",
                FilterOperator::Is,
                FilterValue::Bool(true),
            ),
            hdr_group(BoolOp::And, "bad name", FilterOperator::Contains, text("a")),
            hdr_group(BoolOp::And, "", FilterOperator::Contains, text("a")),
            hdr_group(
                BoolOp::And,
                "List-Id",
                FilterOperator::Gt,
                FilterValue::Number(1),
            ),
            hdr_group(
                BoolOp::And,
                "List-Id",
                FilterOperator::In,
                FilterValue::List(vec![]),
            ),
            hdr_group(BoolOp::And, "List-Id", FilterOperator::Contains, text("")),
            hdr_group(
                BoolOp::And,
                "List-Id",
                FilterOperator::Contains,
                FilterValue::Bool(true),
            ),
            hdr_group(BoolOp::And, "List-Id", FilterOperator::Is, text("x")),
        ];
        for g in &bad {
            assert!(
                db.count_group_matches(g, &SmartMailboxScope::default(), 100)
                    .is_err(),
                "{g:?}"
            );
            assert!(
                db.upsert_smart_mailbox(SmartMailboxUpsert {
                    id: None,
                    name: "Bad",
                    query: "",
                    symbol: None,
                    group: Some(g),
                    scope: None,
                })
                .is_err(),
                "upsert must store nothing for {g:?}"
            );
        }
        assert!(db.list_smart_mailboxes().unwrap().is_empty());
    }

    #[test]
    fn re_upserting_a_descriptor_row_keeps_captured_headers() {
        let db = fresh_db();
        seed_headers(&db);
        seed_msg(&db, 1, "a@x.test", "M1 again"); // ON CONFLICT DO UPDATE, not REPLACE
        let g = hdr_group(
            BoolOp::And,
            "List-Id",
            FilterOperator::Contains,
            text("dev."),
        );
        assert_eq!(tag_count(&db, &g), 1);
        assert_eq!(db.message_header_coverage().unwrap().captured, 3);
    }

    /// Perf guard (MM-1c): a Header condition must be driven by the
    /// `idx_message_headers_name` index (or the header_names UNIQUE index),
    /// never a full scan of `message_headers`; "absent" may walk the capture
    /// table's primary key but never scans `message_headers`.
    #[test]
    fn header_conditions_never_full_scan_the_header_table() {
        let db = fresh_db();
        seed_headers(&db);
        for (op, v) in [
            (FilterOperator::Contains, text("dev")),
            (FilterOperator::Is, FilterValue::Bool(true)),
            (FilterOperator::Is, FilterValue::Bool(false)),
        ] {
            let g = hdr_group(BoolOp::And, "List-Id", op, v);
            let mut params = Vec::new();
            let where_sql = crate::filter_sql::compile_group(&g, &mut params).unwrap();
            let conn = db.lock_conn();
            let mut st = conn
                .prepare(&format!(
                    "EXPLAIN QUERY PLAN SELECT COUNT(*) FROM messages m WHERE {where_sql}"
                ))
                .unwrap();
            let plan: Vec<String> = st
                .query_map(rusqlite::params_from_iter(params.iter()), |r| {
                    r.get::<_, String>(3)
                })
                .unwrap()
                .map(Result::unwrap)
                .collect();
            let plan = plan.join(" | ");
            assert!(
                !plan.contains("SCAN message_headers") && !plan.contains("SCAN h"),
                "{op:?}: {plan}"
            );
            assert!(plan.contains("message_headers"), "{op:?}: {plan}");
        }
    }

    /// SECURITY: a partial capture (name dictionary was full) is UNKNOWN, so
    /// "header absent" must not match it.
    #[test]
    fn partial_capture_never_counts_as_header_absent() {
        let db = fresh_db();
        seed_headers(&db);
        let absent = hdr_group(
            BoolOp::And,
            "List-Id",
            FilterOperator::Is,
            FilterValue::Bool(false),
        );
        assert_eq!(tag_count(&db, &absent), 1, "m3 is captured and complete");
        db.lock_conn()
            .execute(
                "UPDATE message_header_capture SET partial = 1 WHERE message_id = 'acc1:INBOX:3'",
                [],
            )
            .unwrap();
        assert_eq!(
            tag_count(&db, &absent),
            0,
            "partial capture is unknown, not absent"
        );
        let present = hdr_group(
            BoolOp::And,
            "List-Id",
            FilterOperator::Is,
            FilterValue::Bool(true),
        );
        assert_eq!(tag_count(&db, &present), 2, "present is unaffected");
    }

    fn complete(db: &Database, id: &str, hs: &[castellum_models::StoredHeader]) {
        db.replace_message_headers(id, hs, castellum_models::HeaderCaptureSource::Body, false)
            .unwrap();
    }
    fn partial(db: &Database, id: &str, hs: &[castellum_models::StoredHeader]) {
        db.replace_message_headers(id, hs, castellum_models::HeaderCaptureSource::Body, true)
            .unwrap();
    }

    /// SECURITY: unknown header state (partial or missing capture) never
    /// matches, under negation and nesting too. Complete captures are exact.
    #[test]
    fn unknown_header_state_never_matches_even_under_negation_and_nesting() {
        let db = fresh_db();
        seed_account_and_mailbox(&db);
        for uid in 1..=5 {
            seed_msg(&db, uid, "a@x.test", &format!("M{uid}"));
        }
        // m1 complete+List-Id dev; m2 complete, no List-Id; m3 partial, no List-Id
        // stored; m4 partial but List-Id dev stored; m5 never captured.
        complete(&db, "acc1:INBOX:1", &[sh("list-id", 0, "dev")]);
        complete(&db, "acc1:INBOX:2", &[]);
        partial(&db, "acc1:INBOX:3", &[]);
        partial(&db, "acc1:INBOX:4", &[sh("list-id", 0, "dev")]);
        let contains = |op, w: &str| hdr_group(op, "List-Id", FilterOperator::Contains, text(w));
        let ids = |g: &FilterGroup| -> Vec<String> {
            let mut v: Vec<String> = db
                .rule_candidates(g, Some("acc1"), None, None, 100)
                .unwrap()
                .into_iter()
                .map(|m| m.id.rsplit(':').next().unwrap().to_string())
                .collect();
            v.sort();
            v
        };
        // Positive: conclusive evidence counts, even on a partial capture.
        assert_eq!(ids(&contains(BoolOp::And, "dev")), ["1", "4"]);
        // None-group: only COMPLETE captures without the header (m2) match; the
        // partial-without-evidence (m3) and uncaptured (m5) messages are unknown.
        assert_eq!(ids(&contains(BoolOp::None, "dev")), ["2"]);
        // Absent: m2 only (m3 partial and m5 uncaptured are unknown; m1/m4 have it).
        let absent = hdr_group(
            BoolOp::And,
            "List-Id",
            FilterOperator::Is,
            FilterValue::Bool(false),
        );
        assert_eq!(ids(&absent), ["2"]);
        // None around absent = "has the header", known only where evidence exists.
        let not_absent = hdr_group(
            BoolOp::None,
            "List-Id",
            FilterOperator::Is,
            FilterValue::Bool(false),
        );
        assert_eq!(ids(&not_absent), ["1", "4"]);
        // Nested NOT: none( and( contains dev ) ) and none( none( contains dev ) ).
        let nested_none = FilterGroup {
            op: BoolOp::None,
            conditions: vec![],
            groups: vec![contains(BoolOp::And, "dev")],
        };
        assert_eq!(ids(&nested_none), ["2"]);
        let double_neg = FilterGroup {
            op: BoolOp::None,
            conditions: vec![],
            groups: vec![nested_none.clone()],
        };
        assert_eq!(
            ids(&double_neg),
            ["1", "4"],
            "unknown stays unknown: m3, m5 excluded"
        );
        // Header leaf mixed with a non-header leaf inside a none group keeps the
        // non-header NULL coercion (sent_at NULL rows) without coercing the header.
        let mixed = FilterGroup {
            op: BoolOp::None,
            conditions: vec![
                cond(
                    FilterField::Header("List-Id".into()),
                    FilterOperator::Contains,
                    text("dev"),
                ),
                cond(
                    FilterField::Subject,
                    FilterOperator::Equals,
                    text("nomatch"),
                ),
            ],
            groups: vec![],
        };
        assert_eq!(ids(&mixed), ["2"]);
        // Parity: the rule engine and the smart-mailbox counter share one SQL
        // path, so counts agree with rule_candidates for every shape above.
        for g in [&nested_none, &double_neg, &mixed, &absent, &not_absent] {
            assert_eq!(
                tag_count(&db, g) as usize,
                ids(g).len(),
                "smart-mailbox count and rule candidates must agree"
            );
        }
    }
    /// SECURITY (review MUST 2): a value cut at HEADER_VALUE_MAX_BYTES (stored
    /// with the cut marker, capture partial) never satisfies `Equals` on its
    /// stub, and `Contains` past the cut is UNKNOWN (also under `none`), while
    /// `Contains` inside the kept prefix is still a conclusive match.
    #[test]
    fn cut_header_value_is_never_a_false_conclusion() {
        let db = fresh_db();
        seed_account_and_mailbox(&db);
        for uid in 1..=2 {
            seed_msg(&db, uid, "a@x.test", &format!("M{uid}"));
        }
        let max = castellum_models::HEADER_VALUE_MAX_BYTES;
        let marker = castellum_models::HEADER_VALUE_CUT_MARKER;
        let stub = "a".repeat(max - marker.len_utf8());
        // m1: a cut X-Long (as the extractor stores it); m2: complete, short.
        partial(
            &db,
            "acc1:INBOX:1",
            &[sh("x-long", 0, &format!("{stub}{marker}"))],
        );
        complete(&db, "acc1:INBOX:2", &[sh("x-long", 0, "short")]);
        let g = |op, operator, v: &str| hdr_group(op, "X-Long", operator, text(v));
        assert_eq!(
            tag_count(&db, &g(BoolOp::And, FilterOperator::Equals, &stub)),
            0,
            "Equals must not match a cut stub"
        );
        assert_eq!(
            tag_count(&db, &g(BoolOp::None, FilterOperator::Equals, &stub)),
            1,
            "none(Equals stub): only the complete m2; the cut m1 is unknown"
        );
        assert_eq!(
            tag_count(&db, &g(BoolOp::And, FilterOperator::Contains, "aaaa")),
            1,
            "Contains inside the kept prefix is conclusive"
        );
        assert_eq!(
            tag_count(&db, &g(BoolOp::None, FilterOperator::Contains, "zzz")),
            1,
            "none(Contains past the cut): m1 is unknown, m2 matches"
        );
    }
    /// NIT (review): a non-header leaf under a `none` group with a header leaf
    /// keeps its old NULL handling at ANY depth. `none(and(List-Id contains
    /// dev, date after X))` must match a message whose List-Id is "dev" but
    /// whose `sent_at` is NULL (the and is false for it, as before MM-1c),
    /// while an unknown header state still never matches.
    #[test]
    fn none_group_keeps_null_safety_for_non_header_leaves_at_depth() {
        let db = fresh_db();
        seed_account_and_mailbox(&db);
        for uid in 1..=3 {
            seed_msg(&db, uid, "a@x.test", &format!("M{uid}"));
        }
        // m1: List-Id dev, sent_at NULL; m2: List-Id dev, sent_at set (recent);
        // m3: never captured, sent_at NULL.
        complete(&db, "acc1:INBOX:1", &[sh("list-id", 0, "dev")]);
        complete(&db, "acc1:INBOX:2", &[sh("list-id", 0, "dev")]);
        db.lock_conn()
            .execute(
                "UPDATE messages SET sent_at = NULL WHERE id IN ('acc1:INBOX:1', 'acc1:INBOX:3')",
                [],
            )
            .unwrap();
        db.lock_conn()
            .execute(
                "UPDATE messages SET sent_at = 2000000000 WHERE id = 'acc1:INBOX:2'",
                [],
            )
            .unwrap();
        let and = FilterGroup {
            op: BoolOp::And,
            conditions: vec![
                cond(
                    FilterField::Header("List-Id".into()),
                    FilterOperator::Contains,
                    text("dev"),
                ),
                cond(
                    FilterField::Date,
                    FilterOperator::After,
                    FilterValue::Number(1_000_000_000),
                ),
            ],
            groups: vec![],
        };
        let none = FilterGroup {
            op: BoolOp::None,
            conditions: vec![],
            groups: vec![and.clone()],
        };
        let ids = |g: &FilterGroup| -> Vec<String> {
            let mut v: Vec<String> = db
                .rule_candidates(g, Some("acc1"), None, None, 100)
                .unwrap()
                .into_iter()
                .map(|m| m.id.rsplit(':').next().unwrap().to_string())
                .collect();
            v.sort();
            v
        };
        // m1: and(1, NULL->0) = 0 -> none matches. m2: and(1, 1) -> excluded.
        // m3: and(unknown, 0) = 0 -> conclusively false whatever the header
        // is, so none matches too (the header cannot change the outcome).
        assert_eq!(ids(&none), ["1", "3"]);
        assert_eq!(tag_count(&db, &none), 2, "count and candidates agree");
        // Two levels deeper (none > or > and) behaves the same.
        let deep = FilterGroup {
            op: BoolOp::None,
            conditions: vec![],
            groups: vec![FilterGroup {
                op: BoolOp::Or,
                conditions: vec![],
                groups: vec![and],
            }],
        };
        assert_eq!(ids(&deep), ["1", "3"]);
        // Where the header DOES decide the outcome, unknown still never matches:
        // none(List-Id contains dev) -> m3 is unknown, m1/m2 have it.
        let plain = hdr_group(
            BoolOp::None,
            "List-Id",
            FilterOperator::Contains,
            text("dev"),
        );
        assert_eq!(ids(&plain), Vec::<String>::new());
    }
    /// SHOULD 6 (review): one pass yields the same total / unread / capped
    /// triple the two-pass version did, header conditions included, exact
    /// below the cap and clamped at it.
    #[test]
    fn group_counts_are_exact_below_the_cap_and_clamped_at_it() {
        let db = fresh_db();
        seed_account_and_mailbox(&db);
        for uid in 1..=5 {
            seed_msg(&db, uid, "a@x.test", &format!("M{uid}"));
            complete(
                &db,
                &format!("acc1:INBOX:{uid}"),
                &[sh("list-id", 0, "dev")],
            );
        }
        for uid in [1, 2] {
            db.lock_conn()
                .execute(
                    "INSERT INTO message_flags (message_id, is_read) VALUES (?1, 1)
                     ON CONFLICT(message_id) DO UPDATE SET is_read = 1",
                    params![format!("acc1:INBOX:{uid}")],
                )
                .unwrap();
        }
        let g = hdr_group(
            BoolOp::And,
            "List-Id",
            FilterOperator::Contains,
            text("dev"),
        );
        let s = SmartMailboxScope::default();
        assert_eq!(db.count_group_matches(&g, &s, 100).unwrap(), (5, 3, false));
        assert_eq!(db.count_group_matches(&g, &s, 4).unwrap(), (4, 3, true));
        assert_eq!(db.count_group_matches(&g, &s, 3).unwrap(), (3, 3, true));
        assert_eq!(db.count_group_matches(&g, &s, 2).unwrap(), (2, 2, true));
    }
}
