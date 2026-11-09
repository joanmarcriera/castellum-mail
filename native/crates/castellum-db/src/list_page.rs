// Castellum - keyset (cursor) message-list paging (S64).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// `list_message_headers`/`list_messages_by_role` (repo.rs) page with
// LIMIT/OFFSET, which walks every skipped row per page -- fine for the
// app's 500-row cap, useless once the app scrolls past it (S64's whole
// point). This module pages with a seek instead: the last row of the
// previous page becomes a `ListCursor`, and the next page's cost is
// independent of how deep it is.
//
// NULL-aware keyset over `ORDER BY m.sent_at DESC, m.id`: SQLite sorts NULL
// `sent_at` LAST under DESC, so a page walks two segments --
//   A: `sent_at` NOT NULL, ordered (sent_at DESC, id ASC);
//   B: `sent_at` NULL, ordered (id ASC) --
// and a cursor in A continues A, spilling into B once A is exhausted; a
// cursor already in B continues B directly. `sent_at <= ?` (not `< ? OR (=
// ? AND …)`) keeps `idx_messages_list`/`idx_messages_sent_at` usable as a
// plain range seek; the `NOT (sent_at = ? AND id <= ?)` term only trims the
// rows already returned that share the cursor's timestamp.
//
// Sorted paging (#3043, slice 2) generalises this without a migration:
//   * Date, either direction, uses the same two segments and the same
//     indexes (ascending scans them backwards); undated rows stay LAST in
//     both directions.
//   * From / Subject have no index. They sort a narrow inner query
//     (`id`, folded key, date) by `castellum_sort_fold` with a top-N sort,
//     and the header joins run only for the page. Ties break by date, newest
//     first, then id; reversing the sort reverses only the primary key.

use crate::repo::{row_to_header, HEADER_COLUMNS};
use crate::sort_fold::sort_fold;
use crate::{Database, DbError};
use castellum_models::MessageHeader;
use rusqlite::types::Value;
use rusqlite::ToSql;
use serde::{Deserialize, Serialize};

/// Sentinel the text-sort queries use for a NULL `sent_at`, so undated rows
/// order as the oldest. Must match between the SQL and the bound cursor.
const NO_DATE: i64 = -9_223_372_036_854_775_807;

/// What the list is ordered by. Date is the default (newest first).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SortKey {
    #[default]
    Date,
    From,
    Subject,
    /// Group by read state: ascending puts UNREAD rows first (`is_read` 0 before 1).
    Unread,
    /// Group by the flag: ascending puts unflagged first, so a first click uses descending.
    Starred,
    /// Group by attachment presence: ascending puts rows without one first.
    Attachments,
}

impl SortKey {
    /// Keys whose value is a 0/1 integer rather than folded text.
    fn is_flag(self) -> bool {
        matches!(
            self,
            SortKey::Unread | SortKey::Starred | SortKey::Attachments
        )
    }
}

/// A list order: a key and a direction. For `Date`, descending (newest
/// first) is the default and the order of the unsorted paging calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListSort {
    pub key: SortKey,
    pub ascending: bool,
}

impl Default for ListSort {
    fn default() -> Self {
        ListSort {
            key: SortKey::Date,
            ascending: false,
        }
    }
}

impl ListSort {
    /// Parse the wire form: `None` or `""` is `date`; an unknown name is
    /// `None` (the caller maps it to an invalid-argument error).
    pub fn parse(key: Option<&str>, ascending: bool) -> Option<ListSort> {
        let key = match key {
            None | Some("") | Some("date") => SortKey::Date,
            Some("from") => SortKey::From,
            Some("subject") => SortKey::Subject,
            Some("unread") => SortKey::Unread,
            Some("starred") => SortKey::Starred,
            Some("attachments") => SortKey::Attachments,
            Some(_) => return None,
        };
        Some(ListSort { key, ascending })
    }

    /// Whether this order is keyed by a column value (folded text, or a 0/1
    /// flag) rather than by date: its cursor carries a `key`.
    pub fn is_keyed(self) -> bool {
        self.key != SortKey::Date
    }
}

/// The last row of a page: enough to seek the next one without an OFFSET.
///
/// `key` is the sort value of that row for a keyed sort (folded text for
/// `From`/`Subject`, "0"/"1" for the flag keys) and absent for `Date`, so date cursors serialise exactly as
/// before. A cursor is only valid for the sort that produced it; a date
/// cursor cannot tell ascending from descending, so callers restart from the
/// first page whenever the sort changes.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ListCursor {
    pub sent_at: Option<i64>,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// One page of message headers plus where to resume. `next == None` means
/// there is nothing more to page through.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct MessagePage {
    pub rows: Vec<MessageHeader>,
    pub next: Option<ListCursor>,
}

/// `WHERE`-clause fragment plus its extra bind values (beyond the base
/// predicate's own params), for one keyset segment.
struct SegmentPredicate {
    sql: &'static str,
    extra: Vec<Value>,
}

/// Dated rows, newest first: `ORDER BY sent_at DESC, id`.
fn segment_a(after: Option<&ListCursor>) -> Option<SegmentPredicate> {
    match after {
        None => Some(SegmentPredicate {
            sql: "AND m.sent_at IS NOT NULL",
            extra: vec![],
        }),
        Some(ListCursor {
            sent_at: Some(s),
            id,
            ..
        }) => Some(SegmentPredicate {
            sql:
                "AND m.sent_at IS NOT NULL AND m.sent_at <= ? AND NOT (m.sent_at = ? AND m.id <= ?)",
            extra: vec![
                Value::Integer(*s),
                Value::Integer(*s),
                Value::Text(id.clone()),
            ],
        }),
        Some(ListCursor { sent_at: None, .. }) => None,
    }
}

/// Dated rows, oldest first: `ORDER BY sent_at ASC, id DESC` (the exact
/// reverse of the index order, so no temp B-tree).
fn segment_a_ascending(after: Option<&ListCursor>) -> Option<SegmentPredicate> {
    match after {
        None => Some(SegmentPredicate {
            sql: "AND m.sent_at IS NOT NULL",
            extra: vec![],
        }),
        Some(ListCursor {
            sent_at: Some(s),
            id,
            ..
        }) => Some(SegmentPredicate {
            sql:
                "AND m.sent_at IS NOT NULL AND m.sent_at >= ? AND NOT (m.sent_at = ? AND m.id >= ?)",
            extra: vec![
                Value::Integer(*s),
                Value::Integer(*s),
                Value::Text(id.clone()),
            ],
        }),
        Some(ListCursor { sent_at: None, .. }) => None,
    }
}

/// Undated rows: always last, in id order, for either date direction.
fn segment_b(after: Option<&ListCursor>) -> SegmentPredicate {
    match after {
        Some(ListCursor {
            sent_at: None, id, ..
        }) => SegmentPredicate {
            sql: "AND m.sent_at IS NULL AND m.id > ?",
            extra: vec![Value::Text(id.clone())],
        },
        _ => SegmentPredicate {
            sql: "AND m.sent_at IS NULL",
            extra: vec![],
        },
    }
}

/// Which messages a page walks: one mailbox, or every mailbox of a role.
#[derive(Clone, Copy)]
enum Scope<'a> {
    Mailbox {
        account_id: &'a str,
        mailbox_id: &'a str,
    },
    Role(&'a str),
}

impl Scope<'_> {
    /// Bind values for the scope's own `?1`/`?2` placeholders.
    fn binds(&self) -> Vec<&dyn ToSql> {
        match self {
            Scope::Mailbox {
                account_id,
                mailbox_id,
            } => vec![account_id, mailbox_id],
            Scope::Role(role) => vec![role],
        }
    }

    /// `FROM … WHERE …` over the full header joins, for the date path.
    fn header_from(&self) -> &'static str {
        match self {
            Scope::Mailbox { .. } => {
                "FROM messages m
             LEFT JOIN message_flags f ON f.message_id = m.id
             LEFT JOIN threads t ON t.id = m.thread_id
             LEFT JOIN message_bodies b ON b.message_id = m.id
             LEFT JOIN snoozed_messages sn ON sn.message_id = m.id
             WHERE m.account_id = ?1 AND m.mailbox_id = ?2 AND sn.message_id IS NULL"
            }
            Scope::Role(_) => {
                "FROM messages m
             JOIN mailboxes mb ON mb.id = m.mailbox_id AND mb.role = ?1
             LEFT JOIN message_flags f ON f.message_id = m.id
             LEFT JOIN threads t ON t.id = m.thread_id
             LEFT JOIN message_bodies b ON b.message_id = m.id
             LEFT JOIN snoozed_messages sn ON sn.message_id = m.id
             WHERE sn.message_id IS NULL"
            }
        }
    }

    /// `FROM … WHERE …` for the narrow inner query of a keyed sort (no
    /// header joins; they run only for the page). The flag keys need the one
    /// `message_flags` join.
    fn narrow_from(&self, flags: bool) -> String {
        let flags_join = if flags {
            "LEFT JOIN message_flags f ON f.message_id = m.id"
        } else {
            ""
        };
        match self {
            Scope::Mailbox { .. } => format!(
                "FROM messages m {flags_join}
                 LEFT JOIN snoozed_messages sn ON sn.message_id = m.id
                 WHERE m.account_id = ?1 AND m.mailbox_id = ?2 AND sn.message_id IS NULL"
            ),
            Scope::Role(_) => format!(
                "FROM messages m
                 JOIN mailboxes mb ON mb.id = m.mailbox_id AND mb.role = ?1 {flags_join}
                 LEFT JOIN snoozed_messages sn ON sn.message_id = m.id
                 WHERE sn.message_id IS NULL"
            ),
        }
    }
}

fn run_headers(
    db: &Database,
    sql: &str,
    leading: &[&dyn ToSql],
    extra: &[Value],
    limit: i64,
) -> Result<Vec<MessageHeader>, DbError> {
    let conn = db.lock_conn();
    let mut stmt = conn.prepare(sql)?;
    let mut params: Vec<&dyn ToSql> = leading.to_vec();
    for v in extra {
        params.push(v);
    }
    params.push(&limit);
    let rows = stmt.query_map(params.as_slice(), row_to_header)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// SQL expression of a text sort's folded key. Must agree with
/// [`row_sort_key`], which builds the cursor key from the last row.
fn key_expr(key: SortKey) -> &'static str {
    match key {
        SortKey::From => {
            "castellum_sort_fold(COALESCE(NULLIF(trim(m.from_name), ''), m.from_address, ''), 0)"
        }
        SortKey::Subject => "castellum_sort_fold(COALESCE(m.subject, ''), 1)",
        SortKey::Unread => "COALESCE(f.is_read, 0)",
        SortKey::Starred => "COALESCE(f.is_starred, 0)",
        SortKey::Attachments => "m.has_attachments",
        SortKey::Date => "''",
    }
}

/// The folded sort key of a header, computed in Rust with the same fold the
/// SQL function uses, so a cursor can never disagree with the query.
fn row_sort_key(key: SortKey, h: &MessageHeader) -> String {
    match key {
        SortKey::From => {
            let name = h
                .from_name
                .as_deref()
                .map(|n| n.trim_matches(' '))
                .filter(|n| !n.is_empty());
            sort_fold(name.or(h.from_address.as_deref()).unwrap_or(""), false)
        }
        SortKey::Subject => sort_fold(h.subject.as_deref().unwrap_or(""), true),
        SortKey::Unread => i32::from(h.is_read).to_string(),
        SortKey::Starred => i32::from(h.is_starred).to_string(),
        SortKey::Attachments => i32::from(h.has_attachments).to_string(),
        SortKey::Date => String::new(),
    }
}

impl Database {
    /// DR1: fills `MessageHeader.to_display` on headers that live in a
    /// recipient-first mailbox (role `drafts` or `sent`), so the list can
    /// show who a message went TO instead of who sent it (always the
    /// account owner there). Two cheap indexed lookups per page; headers in
    /// every other mailbox are left `None` and the query is skipped
    /// entirely when none of the page is in such a mailbox.
    pub(crate) fn attach_recipients(&self, headers: &mut [MessageHeader]) -> Result<(), DbError> {
        if headers.is_empty() {
            return Ok(());
        }
        let conn = self.lock_conn();
        let mut mailbox_ids: Vec<&str> = headers.iter().map(|h| h.mailbox_id.as_str()).collect();
        mailbox_ids.sort_unstable();
        mailbox_ids.dedup();
        let mut recipient_first = std::collections::HashSet::new();
        for mb in &mailbox_ids {
            let role: Option<String> = conn
                .query_row("SELECT role FROM mailboxes WHERE id = ?1", [mb], |r| {
                    r.get(0)
                })
                .ok()
                .flatten();
            if matches!(role.as_deref(), Some("drafts" | "sent")) {
                recipient_first.insert(mb.to_string());
            }
        }
        if recipient_first.is_empty() {
            return Ok(());
        }
        let mut stmt = conn.prepare(
            "SELECT name, address FROM message_addresses
             WHERE message_id = ?1 AND kind = 'to' ORDER BY ordinal",
        )?;
        for h in headers
            .iter_mut()
            .filter(|h| recipient_first.contains(&h.mailbox_id))
        {
            let names = stmt
                .query_map([&h.id], |r| {
                    let name: Option<String> = r.get(0)?;
                    let address: String = r.get(1)?;
                    Ok(name.filter(|n| !n.trim().is_empty()).unwrap_or(address))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            h.to_display = Some(names.join(", "));
        }
        Ok(())
    }

    /// Page through one mailbox newest-first, keyset-style. `after` is the
    /// `next` cursor from the previous call (`None` for the first page).
    /// Same row shape, ordering and snooze exclusion as
    /// `list_message_headers`; cost is independent of how deep the page is.
    pub fn list_message_page(
        &self,
        account_id: &str,
        mailbox_id: &str,
        after: Option<&ListCursor>,
        limit: i64,
    ) -> Result<MessagePage, DbError> {
        self.list_message_page_sorted(account_id, mailbox_id, ListSort::default(), after, limit)
    }

    /// Same shape as [`Self::list_message_page`], scoped by mailbox role
    /// across every account (the unified-inbox list), using
    /// `idx_messages_sent_at`.
    pub fn list_role_page(
        &self,
        role: &str,
        after: Option<&ListCursor>,
        limit: i64,
    ) -> Result<MessagePage, DbError> {
        self.list_role_page_sorted(role, ListSort::default(), after, limit)
    }

    /// [`Self::list_message_page`] in an arbitrary [`ListSort`]. `after` must
    /// come from a page of the SAME sort (a text sort rejects a cursor with
    /// no `key`, and a date sort ignores one).
    pub fn list_message_page_sorted(
        &self,
        account_id: &str,
        mailbox_id: &str,
        sort: ListSort,
        after: Option<&ListCursor>,
        limit: i64,
    ) -> Result<MessagePage, DbError> {
        let scope = Scope::Mailbox {
            account_id,
            mailbox_id,
        };
        self.page_sorted(scope, sort, after, limit)
    }

    /// [`Self::list_role_page`] in an arbitrary [`ListSort`].
    pub fn list_role_page_sorted(
        &self,
        role: &str,
        sort: ListSort,
        after: Option<&ListCursor>,
        limit: i64,
    ) -> Result<MessagePage, DbError> {
        self.page_sorted(Scope::Role(role), sort, after, limit)
    }

    fn page_sorted(
        &self,
        scope: Scope<'_>,
        sort: ListSort,
        after: Option<&ListCursor>,
        limit: i64,
    ) -> Result<MessagePage, DbError> {
        // SQLite reads a negative `LIMIT` as "no limit": a stray -1 would
        // return the whole scope in one page. Every core caller already
        // clamps; this keeps the public db API from depending on that.
        let limit = limit.max(0);
        let mut rows = if sort.is_keyed() {
            self.keyed_sorted_rows(scope, sort, after, limit)?
        } else {
            self.date_sorted_rows(scope, sort.ascending, after, limit)?
        };
        self.attach_recipients(&mut rows)?;
        // Tags travel with the row (FP41), as in `list_message_headers`;
        // the row queries have released the connection lock by now.
        self.attach_keywords(&mut rows)?;
        let next = next_cursor(&rows, limit, sort);
        Ok(MessagePage { rows, next })
    }

    /// Date order in either direction: the dated segment, then (once it is
    /// exhausted) the undated one, which is last in both directions.
    fn date_sorted_rows(
        &self,
        scope: Scope<'_>,
        ascending: bool,
        after: Option<&ListCursor>,
        limit: i64,
    ) -> Result<Vec<MessageHeader>, DbError> {
        let base_from = scope.header_from();
        let leading = scope.binds();
        let (segment, order) = if ascending {
            (
                segment_a_ascending(after),
                "ORDER BY m.sent_at ASC, m.id DESC",
            )
        } else {
            (segment_a(after), "ORDER BY m.sent_at DESC, m.id")
        };
        let mut rows = Vec::new();
        if let Some(pred) = segment {
            let sql = format!(
                "SELECT {HEADER_COLUMNS} {base_from} {} {order} LIMIT ?",
                pred.sql
            );
            rows = run_headers(self, &sql, &leading, &pred.extra, limit)?;
        }
        let remaining = limit - rows.len() as i64;
        if remaining > 0 {
            let pred = segment_b(after);
            let sql = format!(
                "SELECT {HEADER_COLUMNS} {base_from} {} ORDER BY m.id LIMIT ?",
                pred.sql
            );
            rows.extend(run_headers(self, &sql, &leading, &pred.extra, remaining)?);
        }
        Ok(rows)
    }

    /// From / Subject / flag order. One scan of the scope with a top-N sort over a
    /// narrow inner query; the header joins run only for the page. Ties break
    /// by date (newest first, undated oldest) then id.
    fn keyed_sorted_rows(
        &self,
        scope: Scope<'_>,
        sort: ListSort,
        after: Option<&ListCursor>,
        limit: i64,
    ) -> Result<Vec<MessageHeader>, DbError> {
        let key = key_expr(sort.key);
        let dir = if sort.ascending { "ASC" } else { "DESC" };
        let cmp = if sort.ascending { ">" } else { "<" };
        let d = format!("COALESCE(m.sent_at, {NO_DATE})");
        let mut extra: Vec<Value> = Vec::new();
        let mut cursor_pred = String::new();
        if let Some(c) = after {
            let Some(k) = c.key.as_ref() else {
                return Err(DbError(
                    "cursor has no sort key: it came from a different sort".into(),
                ));
            };
            let sent = c.sent_at.unwrap_or(NO_DATE);
            // A flag key compares as an integer: a bound TEXT never equals an integer
            // expression in SQLite, so the cursor value must be bound with its type.
            let bind = |k: &str| -> Result<Value, DbError> {
                if sort.key.is_flag() {
                    k.parse::<i64>()
                        .map(Value::Integer)
                        .map_err(|_| DbError("cursor key is not a flag value".into()))
                } else {
                    Ok(Value::Text(k.to_string()))
                }
            };
            cursor_pred = format!(
                "AND ({key} {cmp} ? OR ({key} = ? AND ({d} < ? OR ({d} = ? AND m.id > ?))))"
            );
            extra = vec![
                bind(k)?,
                bind(k)?,
                Value::Integer(sent),
                Value::Integer(sent),
                Value::Text(c.id.clone()),
            ];
        }
        let narrow_from = scope.narrow_from(matches!(sort.key, SortKey::Unread | SortKey::Starred));
        let sql = format!(
            "SELECT {HEADER_COLUMNS}
             FROM (SELECT m.id AS kid, {key} AS k, {d} AS d
                   {narrow_from} {cursor_pred}
                   ORDER BY k {dir}, d DESC, kid ASC LIMIT ?) page
             JOIN messages m ON m.id = page.kid
             LEFT JOIN message_flags f ON f.message_id = m.id
             LEFT JOIN threads t ON t.id = m.thread_id
             LEFT JOIN message_bodies b ON b.message_id = m.id
             ORDER BY page.k {dir}, page.d DESC, page.kid ASC"
        );
        run_headers(self, &sql, &scope.binds(), &extra, limit)
    }
}

/// A full page (exactly `limit` rows) implies there may be more; anything
/// short of that is the end.
fn next_cursor(rows: &[MessageHeader], limit: i64, sort: ListSort) -> Option<ListCursor> {
    if rows.len() as i64 != limit {
        return None;
    }
    rows.last().map(|h| ListCursor {
        sent_at: h.sent_at,
        id: h.id.clone(),
        key: sort.is_keyed().then(|| row_sort_key(sort.key, h)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use castellum_models::{Account, Mailbox, NewMessage};

    const ACCOUNT_ID: &str = "acct-page";
    const MAILBOX_ID: &str = "acct-page:INBOX";
    /// 1,237 messages: 20 with a NULL `sent_at`, 40 sharing a duplicated
    /// timestamp (a run of 2, twenty times), 7 snoozed.
    const TOTAL: i64 = 1_237;
    const NULL_COUNT: i64 = 20;
    const SNOOZED_COUNT: i64 = 7;

    /// Unique per RUN, not just per pid (see `lib.rs::tests::tmp_db`'s doc
    /// comment: pid+counter alone reopens an earlier run's leftover file).
    fn tmp_db_path() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        castellum_testutil::scratch_root().join(format!(
            "castellum-db-list-page-test-{}-{stamp}-{n}.db",
            std::process::id()
        ))
    }

    /// The account and INBOX of [`seeded_db`], with no messages.
    fn empty_db() -> Database {
        let db = Database::open(&tmp_db_path()).expect("open");
        db.upsert_account(&Account {
            id: ACCOUNT_ID.to_string(),
            email: "acct-page@test".to_string(),
            display_name: None,
            provider: "imap".to_string(),
        })
        .expect("upsert_account");
        db.upsert_mailbox(&Mailbox {
            id: MAILBOX_ID.to_string(),
            account_id: ACCOUNT_ID.to_string(),
            path: "INBOX".to_string(),
            name: "INBOX".to_string(),
            role: Some("inbox".to_string()),
            delimiter: Some("/".to_string()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .expect("upsert_mailbox");
        db
    }

    fn seeded_db() -> Database {
        let db = empty_db();
        let base_sent_at = 1_700_000_000i64;
        for i in 0..TOTAL {
            let id = format!("{MAILBOX_ID}:{i}");
            // Last NULL_COUNT ids (oldest-inserted-uid-wise, i.e. the highest
            // i, is irrelevant here) get a NULL sent_at; every 31st non-null
            // row duplicates the previous timestamp (creates predictable runs
            // without lining up with the NULL stride).
            let sent_at = if i < NULL_COUNT {
                None
            } else if i % 31 == 1 {
                Some(base_sent_at - (i - 1))
            } else {
                Some(base_sent_at - i)
            };
            db.upsert_message(&NewMessage {
                id: id.clone(),
                account_id: ACCOUNT_ID.to_string(),
                mailbox_id: MAILBOX_ID.to_string(),
                provider_uid: Some(i + 1),
                rfc822_message_id: Some(format!("<{id}@test>")),
                subject: Some(format!("Message {i}")),
                from_name: None,
                from_address: Some("sender@test".to_string()),
                sent_at,
                snippet: None,
                size_bytes: Some(1024),
                has_attachments: false,
                attachments: None,
                is_read: true,
                is_starred: false,
                addresses: vec![],
                text_body: None,
                html_body: None,
                sanitized_html: None,
                has_remote_content: false,
                references: vec![],
            })
            .expect("upsert_message");
        }
        for i in 0..SNOOZED_COUNT {
            db.snooze_message(&format!("{MAILBOX_ID}:{i}"), 4_000_000_000)
                .expect("snooze_message");
        }
        db
    }

    fn all_pages(db: &Database, page_size: i64) -> Vec<MessageHeader> {
        let mut out = Vec::new();
        let mut cursor = None;
        loop {
            let page = db
                .list_message_page(ACCOUNT_ID, MAILBOX_ID, cursor.as_ref(), page_size)
                .expect("list_message_page");
            out.extend(page.rows);
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        out
    }

    fn insert_dated(db: &Database, n: i64, sent_at: i64) {
        let id = format!("{MAILBOX_ID}:{n}");
        db.upsert_message(&NewMessage {
            id: id.clone(),
            account_id: ACCOUNT_ID.to_string(),
            mailbox_id: MAILBOX_ID.to_string(),
            provider_uid: Some(n + 1),
            rfc822_message_id: Some(format!("<{id}@test>")),
            subject: Some(format!("Message {n}")),
            from_name: None,
            from_address: Some("sender@test".to_string()),
            sent_at: Some(sent_at),
            snippet: None,
            size_bytes: Some(1024),
            has_attachments: false,
            attachments: None,
            is_read: true,
            is_starred: false,
            addresses: vec![],
            text_body: None,
            html_body: None,
            sanitized_html: None,
            has_remote_content: false,
            references: vec![],
        })
        .expect("upsert_message");
    }

    fn page_ids(
        db: &Database,
        after: Option<&ListCursor>,
        limit: i64,
    ) -> (Vec<String>, Option<ListCursor>) {
        let page = db
            .list_message_page(ACCOUNT_ID, MAILBOX_ID, after, limit)
            .expect("list_message_page");
        (page.rows.into_iter().map(|h| h.id).collect(), page.next)
    }

    fn ids(ns: impl IntoIterator<Item = i64>) -> Vec<String> {
        ns.into_iter()
            .map(|n| format!("{MAILBOX_ID}:{n}"))
            .collect()
    }

    #[test]
    fn keyset_pages_at_the_boundaries() {
        let db = empty_db();
        let (rows, next) = page_ids(&db, None, 10);
        assert!(
            rows.is_empty() && next.is_none(),
            "empty mailbox: one empty last page"
        );

        // Exactly one page of rows: the full page still offers a cursor
        // (it cannot know nothing follows), and that cursor leads to an
        // empty final page with no cursor.
        for n in 0..10 {
            insert_dated(&db, n, 10_000 - n);
        }
        let (rows, next) = page_ids(&db, None, 10);
        assert_eq!(rows, ids(0..10));
        let next = next.expect("a full page offers a next cursor");
        let (rows, last) = page_ids(&db, Some(&next), 10);
        assert!(
            rows.is_empty() && last.is_none(),
            "page after an exact fit is empty and final"
        );

        // A zero or negative limit is an empty page, never SQLite's
        // `LIMIT -1` ("no limit"), which would return the whole mailbox.
        for limit in [0, -1, i64::MIN] {
            let (rows, next) = page_ids(&db, None, limit);
            assert!(
                rows.is_empty(),
                "limit {limit} returned {} rows",
                rows.len()
            );
            assert!(next.is_none(), "limit {limit} offered a cursor");
        }

        // One row past the page: the last page is partial and final.
        insert_dated(&db, 10, 10_000 - 10);
        let (first, next) = page_ids(&db, None, 10);
        assert_eq!(first, ids(0..10));
        let (rows, last) = page_ids(&db, next.as_ref(), 10);
        assert_eq!(rows, ids([10]));
        assert!(last.is_none());
    }

    #[test]
    fn rows_changing_between_pages_follow_the_cursor_not_an_offset() {
        let db = empty_db();
        for n in 0..25 {
            insert_dated(&db, n, 10_000 - n);
        }
        let (first, next) = page_ids(&db, None, 10);
        assert_eq!(first, ids(0..10));
        let cursor = next.expect("cursor");

        // Between pages: new mail arrives at the top, the row the cursor
        // names is deleted, a later row is redated to the top, and an
        // already-listed row is redated below everything.
        insert_dated(&db, 100, 20_000);
        db.purge_message(ACCOUNT_ID, &format!("{MAILBOX_ID}:9"))
            .expect("purge cursor row");
        insert_dated(&db, 15, 30_000);
        insert_dated(&db, 3, 1_000);

        let mut rest = Vec::new();
        let mut after = Some(cursor);
        while let Some(c) = after {
            let (rows, next) = page_ids(&db, Some(&c), 10);
            rest.extend(rows);
            after = next;
        }
        // The cursor is a (date, id) position, so a deleted cursor row costs
        // nothing; rows that moved above it are not seen again and rows that
        // moved below it are seen (again). No untouched row is lost or doubled.
        let mut want = ids((10..25).filter(|&n| n != 15));
        want.extend(ids([3]));
        assert_eq!(rest, want);
    }

    #[test]
    fn paged_rows_carry_their_tags_without_hidden_keywords() {
        // FP41: reopening a mailbox goes through these paged listings; a
        // tagged message must come back with its tag, never a system keyword.
        let db = seeded_db();
        let tagged = format!("{MAILBOX_ID}:{}", TOTAL - 1);
        db.replace_server_keywords(&tagged, &["Feina".to_string(), "$Forwarded".to_string()])
            .expect("replace_server_keywords");
        let find = |rows: Vec<MessageHeader>| {
            rows.into_iter()
                .find(|h| h.id == tagged)
                .expect("tagged row listed")
                .keywords
        };
        assert_eq!(find(all_pages(&db, 500)), vec!["Feina".to_string()]);
        let mut role_rows = Vec::new();
        let mut cursor = None;
        loop {
            let page = db
                .list_role_page("inbox", cursor.as_ref(), 500)
                .expect("list_role_page");
            role_rows.extend(page.rows);
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(find(role_rows), vec!["Feina".to_string()]);
    }

    #[test]
    fn pages_concatenate_to_the_offset_listing() {
        let db = seeded_db();
        let paged = all_pages(&db, 100);
        let offset_listing = db
            .list_message_headers(ACCOUNT_ID, MAILBOX_ID, TOTAL + 10, 0)
            .expect("list_message_headers");

        assert_eq!(paged.len(), offset_listing.len());
        assert_eq!(
            paged.len() as i64,
            TOTAL - SNOOZED_COUNT,
            "snoozed messages must be excluded from every page"
        );
        let paged_ids: Vec<&str> = paged.iter().map(|h| h.id.as_str()).collect();
        let offset_ids: Vec<&str> = offset_listing.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(paged_ids, offset_ids, "same order, no duplicates, no gaps");

        let unique: std::collections::HashSet<&str> = paged_ids.iter().copied().collect();
        assert_eq!(
            unique.len(),
            paged_ids.len(),
            "no duplicate rows across pages"
        );
    }

    #[test]
    fn page_boundary_inside_a_duplicate_timestamp_run() {
        let db = seeded_db();
        // Page size chosen so a boundary lands inside a 2-row duplicate run
        // (rows are 0-indexed from the newest; row 30/31 is a duplicate pair
        // starting at global row NULL_COUNT-complement -- easiest to just
        // prove correctness empirically: every id appears exactly once
        // across differently-sized pagings).
        let with_31 = all_pages(&db, 31);
        let with_100 = all_pages(&db, 100);
        let ids_31: Vec<&str> = with_31.iter().map(|h| h.id.as_str()).collect();
        let ids_100: Vec<&str> = with_100.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids_31, ids_100);
    }

    #[test]
    fn null_segment_is_reached_and_finished() {
        let db = seeded_db();
        let paged = all_pages(&db, 50);
        let null_count = paged.iter().filter(|h| h.sent_at.is_none()).count();
        assert_eq!(
            null_count as i64,
            NULL_COUNT - SNOOZED_COUNT.min(NULL_COUNT)
        );
    }

    #[test]
    fn explain_uses_list_index_without_temp_btree() {
        let db = seeded_db();
        let cursor = ListCursor {
            sent_at: Some(1_699_999_000),
            id: format!("{MAILBOX_ID}:500"),
            key: None,
        };
        let pred = segment_a(Some(&cursor)).expect("cursor in segment A");
        let sql = format!(
            "EXPLAIN QUERY PLAN SELECT {HEADER_COLUMNS}
             FROM messages m
             LEFT JOIN message_flags f ON f.message_id = m.id
             LEFT JOIN threads t ON t.id = m.thread_id
             LEFT JOIN message_bodies b ON b.message_id = m.id
             LEFT JOIN snoozed_messages sn ON sn.message_id = m.id
             WHERE m.account_id = ?1 AND m.mailbox_id = ?2 AND sn.message_id IS NULL {}
             ORDER BY m.sent_at DESC, m.id LIMIT ?",
            pred.sql
        );
        let conn = db.lock_conn();
        let mut stmt = conn.prepare(&sql).expect("prepare EXPLAIN");
        let mut params: Vec<&dyn ToSql> = vec![&ACCOUNT_ID, &MAILBOX_ID];
        for v in &pred.extra {
            params.push(v);
        }
        let limit: i64 = 50;
        params.push(&limit);
        let plan: Vec<String> = stmt
            .query_map(params.as_slice(), |r| r.get::<_, String>(3))
            .expect("query EXPLAIN")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect EXPLAIN rows");
        let plan_text = plan.join("\n");
        assert!(
            plan_text.contains("idx_messages_list"),
            "expected idx_messages_list in plan:\n{plan_text}"
        );
        assert!(
            !plan_text.contains("USE TEMP B-TREE"),
            "keyset seek must not need a temp B-tree:\n{plan_text}"
        );

        // Role query uses idx_messages_sent_at.
        let role_sql = format!(
            "EXPLAIN QUERY PLAN SELECT {HEADER_COLUMNS}
             FROM messages m
             JOIN mailboxes mb ON mb.id = m.mailbox_id AND mb.role = ?1
             LEFT JOIN message_flags f ON f.message_id = m.id
             LEFT JOIN threads t ON t.id = m.thread_id
             LEFT JOIN message_bodies b ON b.message_id = m.id
             LEFT JOIN snoozed_messages sn ON sn.message_id = m.id
             WHERE sn.message_id IS NULL {}
             ORDER BY m.sent_at DESC, m.id LIMIT ?",
            pred.sql
        );
        let mut role_stmt = conn.prepare(&role_sql).expect("prepare role EXPLAIN");
        let role = "inbox";
        let mut role_params: Vec<&dyn ToSql> = vec![&role];
        for v in &pred.extra {
            role_params.push(v);
        }
        role_params.push(&limit);
        let role_plan: Vec<String> = role_stmt
            .query_map(role_params.as_slice(), |r| r.get::<_, String>(3))
            .expect("query role EXPLAIN")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect role EXPLAIN rows");
        let role_plan_text = role_plan.join("\n");
        assert!(
            role_plan_text.contains("idx_messages_sent_at"),
            "expected idx_messages_sent_at in role plan:\n{role_plan_text}"
        );
        assert!(!role_plan_text.contains("USE TEMP B-TREE"));
    }

    #[test]
    fn role_pages_span_accounts() {
        let db = seeded_db();
        db.upsert_account(&Account {
            id: "acct-page-2".to_string(),
            email: "acct-page-2@test".to_string(),
            display_name: None,
            provider: "imap".to_string(),
        })
        .expect("upsert_account 2");
        db.upsert_mailbox(&Mailbox {
            id: "acct-page-2:INBOX".to_string(),
            account_id: "acct-page-2".to_string(),
            path: "INBOX".to_string(),
            name: "INBOX".to_string(),
            role: Some("inbox".to_string()),
            delimiter: Some("/".to_string()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .expect("upsert_mailbox 2");
        db.upsert_message(&NewMessage {
            id: "acct-page-2:INBOX:1".to_string(),
            account_id: "acct-page-2".to_string(),
            mailbox_id: "acct-page-2:INBOX".to_string(),
            provider_uid: Some(1),
            rfc822_message_id: Some("<other@test>".to_string()),
            subject: Some("Other account".to_string()),
            from_name: None,
            from_address: Some("sender@test".to_string()),
            sent_at: Some(1_700_000_001),
            snippet: None,
            size_bytes: Some(1024),
            has_attachments: false,
            attachments: None,
            is_read: true,
            is_starred: false,
            addresses: vec![],
            text_body: None,
            html_body: None,
            sanitized_html: None,
            has_remote_content: false,
            references: vec![],
        })
        .expect("upsert_message 2");

        let mut cursor = None;
        let mut seen_accounts = std::collections::HashSet::new();
        loop {
            let page = db
                .list_role_page("inbox", cursor.as_ref(), 500)
                .expect("list_role_page");
            for row in &page.rows {
                seen_accounts.insert(row.account_id.clone());
            }
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert!(seen_accounts.contains(ACCOUNT_ID));
        assert!(seen_accounts.contains("acct-page-2"));
    }

    // ---- sorted paging (#3043, slice 2) ----

    const TEXT_ACCOUNT: &str = "acct-sort";
    const TEXT_MAILBOX: &str = "acct-sort:INBOX";
    const TEXT_ROWS: i64 = 150;

    fn text_msg(i: i64, mailbox: &str, account: &str) -> NewMessage {
        let names = [
            Some("Àlex"),
            Some("Zoe"),
            Some("óscar"),
            Some("Anna"),
            None,
            Some("  "),
            Some("Ñu"),
        ];
        let subjects = [
            Some("Re: hola"),
            Some("hola"),
            Some("Zeta"),
            Some("fwd: Àrbre"),
            None,
            Some("RE: RE: apple"),
        ];
        // A run of equal timestamps (every 4th), and some undated rows.
        let sent_at = if i % 11 == 0 {
            None
        } else {
            Some(1_700_000_000 - (i / 4) * 10)
        };
        let id = format!("{mailbox}:{i}");
        NewMessage {
            id: id.clone(),
            account_id: account.to_string(),
            mailbox_id: mailbox.to_string(),
            provider_uid: Some(i + 1),
            rfc822_message_id: Some(format!("<{id}@t>")),
            subject: subjects[(i as usize) % subjects.len()].map(String::from),
            from_name: names[(i as usize) % names.len()].map(String::from),
            from_address: Some(format!("addr{}@test", i % 5)),
            sent_at,
            snippet: None,
            size_bytes: Some(1),
            has_attachments: i % 4 == 1,
            attachments: None,
            is_read: i % 3 != 0,
            is_starred: i % 5 == 0,
            addresses: vec![],
            text_body: None,
            html_body: None,
            sanitized_html: None,
            has_remote_content: false,
            references: vec![],
        }
    }

    fn seed_text_mailbox(db: &Database, account: &str, mailbox: &str, role: &str) {
        db.upsert_account(&Account {
            id: account.to_string(),
            email: format!("{account}@test"),
            display_name: None,
            provider: "imap".to_string(),
        })
        .expect("upsert_account");
        db.upsert_mailbox(&Mailbox {
            id: mailbox.to_string(),
            account_id: account.to_string(),
            path: "INBOX".to_string(),
            name: "INBOX".to_string(),
            role: Some(role.to_string()),
            delimiter: Some("/".to_string()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .expect("upsert_mailbox");
        for i in 0..TEXT_ROWS {
            db.upsert_message(&text_msg(i, mailbox, account))
                .expect("upsert_message");
        }
    }

    fn text_db() -> Database {
        let db = Database::open(&tmp_db_path()).expect("open");
        seed_text_mailbox(&db, TEXT_ACCOUNT, TEXT_MAILBOX, "inbox");
        // Snoozed rows must not appear in any order.
        for i in [3, 40, 77] {
            db.snooze_message(&format!("{TEXT_MAILBOX}:{i}"), 4_000_000_000)
                .expect("snooze");
        }
        db
    }

    fn sorted_pages(db: &Database, sort: ListSort, page_size: i64) -> Vec<MessageHeader> {
        let mut out = Vec::new();
        let mut cursor: Option<ListCursor> = None;
        loop {
            let page = db
                .list_message_page_sorted(
                    TEXT_ACCOUNT,
                    TEXT_MAILBOX,
                    sort,
                    cursor.as_ref(),
                    page_size,
                )
                .expect("sorted page");
            out.extend(page.rows);
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        out
    }

    /// The order the SQL must produce, computed independently in Rust.
    fn reference_order(rows: &[MessageHeader], sort: ListSort) -> Vec<String> {
        let mut v: Vec<&MessageHeader> = rows.iter().collect();
        v.sort_by(|a, b| {
            let primary = match sort.key {
                SortKey::Date => {
                    // Undated last in BOTH directions; ties: id ascending when
                    // descending, id descending when ascending.
                    return match (a.sent_at, b.sent_at) {
                        (None, None) => a.id.cmp(&b.id),
                        (None, Some(_)) => std::cmp::Ordering::Greater,
                        (Some(_), None) => std::cmp::Ordering::Less,
                        (Some(x), Some(y)) if sort.ascending => {
                            x.cmp(&y).then_with(|| b.id.cmp(&a.id))
                        }
                        (Some(x), Some(y)) => y.cmp(&x).then_with(|| a.id.cmp(&b.id)),
                    };
                }
                _ => {
                    let ka = row_sort_key(sort.key, a);
                    let kb = row_sort_key(sort.key, b);
                    if sort.ascending {
                        ka.cmp(&kb)
                    } else {
                        kb.cmp(&ka)
                    }
                }
            };
            let da = a.sent_at.unwrap_or(NO_DATE);
            let db_ = b.sent_at.unwrap_or(NO_DATE);
            primary
                .then_with(|| db_.cmp(&da))
                .then_with(|| a.id.cmp(&b.id))
        });
        v.into_iter().map(|h| h.id.clone()).collect()
    }

    #[test]
    fn sorted_pages_concatenate_to_a_full_sort() {
        let db = text_db();
        let everything = sorted_pages(&db, ListSort::default(), 1000);
        assert_eq!(everything.len() as i64, TEXT_ROWS - 3, "snoozed excluded");
        for sort in [
            ListSort {
                key: SortKey::Date,
                ascending: true,
            },
            ListSort {
                key: SortKey::Date,
                ascending: false,
            },
            ListSort {
                key: SortKey::From,
                ascending: true,
            },
            ListSort {
                key: SortKey::From,
                ascending: false,
            },
            ListSort {
                key: SortKey::Subject,
                ascending: true,
            },
            ListSort {
                key: SortKey::Subject,
                ascending: false,
            },
            ListSort {
                key: SortKey::Unread,
                ascending: true,
            },
            ListSort {
                key: SortKey::Unread,
                ascending: false,
            },
            ListSort {
                key: SortKey::Starred,
                ascending: true,
            },
            ListSort {
                key: SortKey::Starred,
                ascending: false,
            },
            ListSort {
                key: SortKey::Attachments,
                ascending: true,
            },
            ListSort {
                key: SortKey::Attachments,
                ascending: false,
            },
        ] {
            let want = reference_order(&everything, sort);
            for page_size in [1, 7, 500] {
                let got: Vec<String> = sorted_pages(&db, sort, page_size)
                    .into_iter()
                    .map(|h| h.id)
                    .collect();
                assert_eq!(got, want, "sort {sort:?} page size {page_size}");
            }
        }
    }

    /// Slice 5: the flag keys group by value, then newest first inside each group,
    /// and a cursor carries the 0/1 value (typed, or SQLite would never match it).
    #[test]
    fn flag_sorts_group_by_value_then_newest_first() {
        let db = text_db();
        let page = db
            .list_message_page_sorted(
                TEXT_ACCOUNT,
                TEXT_MAILBOX,
                ListSort {
                    key: SortKey::Unread,
                    ascending: true,
                },
                None,
                5,
            )
            .unwrap();
        assert!(
            page.rows.iter().all(|h| !h.is_read),
            "unread rows come first"
        );
        let dates: Vec<i64> = page
            .rows
            .iter()
            .map(|h| h.sent_at.unwrap_or(NO_DATE))
            .collect();
        assert!(
            dates.windows(2).all(|w| w[0] >= w[1]),
            "newest first inside the group"
        );
        let next = page.next.expect("more rows");
        assert_eq!(next.key.as_deref(), Some("0"));

        let starred = db
            .list_message_page_sorted(
                TEXT_ACCOUNT,
                TEXT_MAILBOX,
                ListSort {
                    key: SortKey::Starred,
                    ascending: false,
                },
                None,
                5,
            )
            .unwrap();
        assert!(
            starred.rows.iter().all(|h| h.is_starred),
            "descending puts the flagged first"
        );
        let attach = db
            .list_message_page_sorted(
                TEXT_ACCOUNT,
                TEXT_MAILBOX,
                ListSort {
                    key: SortKey::Attachments,
                    ascending: false,
                },
                None,
                5,
            )
            .unwrap();
        assert!(attach.rows.iter().all(|h| h.has_attachments));
    }

    #[test]
    fn flag_sort_rejects_a_non_numeric_cursor_key() {
        let db = text_db();
        let bad = ListCursor {
            sent_at: Some(1),
            id: "x".into(),
            key: Some("not a number".into()),
        };
        let err = db.list_message_page_sorted(
            TEXT_ACCOUNT,
            TEXT_MAILBOX,
            ListSort {
                key: SortKey::Unread,
                ascending: true,
            },
            Some(&bad),
            10,
        );
        assert!(err.is_err());
    }

    #[test]
    fn flag_sorted_role_pages_span_the_scope() {
        let db = text_db();
        let mut cursor = None;
        let mut seen = 0;
        loop {
            let page = db
                .list_role_page_sorted(
                    "inbox",
                    ListSort {
                        key: SortKey::Starred,
                        ascending: false,
                    },
                    cursor.as_ref(),
                    50,
                )
                .unwrap();
            seen += page.rows.len();
            match page.next {
                Some(n) => cursor = Some(n),
                None => break,
            }
        }
        assert_eq!(
            seen as i64,
            TEXT_ROWS - 3,
            "every inbox row exactly once, snoozed excluded"
        );
    }

    #[test]
    fn parse_accepts_the_flag_keys() {
        for (name, key) in [
            ("unread", SortKey::Unread),
            ("starred", SortKey::Starred),
            ("attachments", SortKey::Attachments),
        ] {
            assert_eq!(ListSort::parse(Some(name), true).unwrap().key, key);
        }
        assert!(ListSort::parse(Some("flag"), true).is_none());
    }

    #[test]
    fn date_ascending_keeps_null_dates_last() {
        let db = seeded_db();
        let mut cursor = None;
        let mut rows = Vec::new();
        loop {
            let page = db
                .list_message_page_sorted(
                    ACCOUNT_ID,
                    MAILBOX_ID,
                    ListSort {
                        key: SortKey::Date,
                        ascending: true,
                    },
                    cursor.as_ref(),
                    100,
                )
                .unwrap();
            rows.extend(page.rows);
            match page.next {
                Some(n) => cursor = Some(n),
                None => break,
            }
        }
        let first_null = rows.iter().position(|h| h.sent_at.is_none()).unwrap();
        assert!(rows[first_null..].iter().all(|h| h.sent_at.is_none()));
        let dated: Vec<i64> = rows[..first_null]
            .iter()
            .filter_map(|h| h.sent_at)
            .collect();
        assert!(dated.windows(2).all(|w| w[0] <= w[1]), "oldest first");
    }

    #[test]
    fn text_sort_folds_accents_and_reply_prefixes() {
        let db = text_db();
        let by_from = sorted_pages(
            &db,
            ListSort {
                key: SortKey::From,
                ascending: true,
            },
            1000,
        );
        let names: Vec<String> = by_from
            .iter()
            .map(|h| row_sort_key(SortKey::From, h))
            .collect();
        assert!(names.windows(2).all(|w| w[0] <= w[1]));
        let first_alex = names.iter().position(|n| n == "alex").unwrap();
        let first_anna = names.iter().position(|n| n == "anna").unwrap();
        let first_zoe = names.iter().position(|n| n == "zoe").unwrap();
        assert!(
            first_alex < first_anna && first_anna < first_zoe,
            "Àlex sorts as alex, before Zoe"
        );
        let by_subject = sorted_pages(
            &db,
            ListSort {
                key: SortKey::Subject,
                ascending: true,
            },
            1000,
        );
        let subjects: Vec<String> = by_subject
            .iter()
            .map(|h| row_sort_key(SortKey::Subject, h))
            .collect();
        assert!(subjects.contains(&"apple".to_string()), "RE: RE: stripped");
        assert!(subjects.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn text_sort_page_boundary_inside_a_run_of_equal_keys() {
        let db = text_db();
        // ~21 rows share each from-name; page size 5 lands boundaries inside the run.
        let sort = ListSort {
            key: SortKey::From,
            ascending: true,
        };
        let a: Vec<String> = sorted_pages(&db, sort, 5)
            .into_iter()
            .map(|h| h.id)
            .collect();
        let b: Vec<String> = sorted_pages(&db, sort, 1000)
            .into_iter()
            .map(|h| h.id)
            .collect();
        assert_eq!(a, b);
        let unique: std::collections::HashSet<&String> = a.iter().collect();
        assert_eq!(unique.len(), a.len(), "no duplicate rows across pages");
    }

    #[test]
    fn role_pages_sorted_span_accounts() {
        let db = text_db();
        seed_text_mailbox(&db, "acct-sort-2", "acct-sort-2:INBOX", "inbox");
        for sort in [
            ListSort {
                key: SortKey::From,
                ascending: true,
            },
            ListSort {
                key: SortKey::Date,
                ascending: true,
            },
        ] {
            let mut rows = Vec::new();
            let mut cursor = None;
            loop {
                let page = db
                    .list_role_page_sorted("inbox", sort, cursor.as_ref(), 40)
                    .unwrap();
                rows.extend(page.rows);
                match page.next {
                    Some(n) => cursor = Some(n),
                    None => break,
                }
            }
            assert_eq!(rows.len() as i64, (TEXT_ROWS - 3) + TEXT_ROWS);
            let accounts: std::collections::HashSet<&str> =
                rows.iter().map(|h| h.account_id.as_str()).collect();
            assert_eq!(accounts.len(), 2);
            let want = reference_order(&rows, sort);
            let got: Vec<String> = rows.into_iter().map(|h| h.id).collect();
            assert_eq!(got, want, "{sort:?}");
        }
    }

    #[test]
    fn cursor_from_another_sort_is_rejected_not_misread() {
        let db = text_db();
        let date_page = db
            .list_message_page_sorted(TEXT_ACCOUNT, TEXT_MAILBOX, ListSort::default(), None, 5)
            .unwrap();
        let date_cursor = date_page.next.unwrap();
        assert!(date_cursor.key.is_none());
        let err = db.list_message_page_sorted(
            TEXT_ACCOUNT,
            TEXT_MAILBOX,
            ListSort {
                key: SortKey::From,
                ascending: true,
            },
            Some(&date_cursor),
            5,
        );
        assert!(err.is_err(), "a date cursor must not seed a text sort");
    }

    #[test]
    fn date_cursor_json_is_unchanged() {
        let c = ListCursor {
            sent_at: Some(5),
            id: "x".into(),
            key: None,
        };
        assert_eq!(
            serde_json::to_string(&c).unwrap(),
            r#"{"sent_at":5,"id":"x"}"#
        );
        let parsed: ListCursor = serde_json::from_str(r#"{"sent_at":5,"id":"x"}"#).unwrap();
        assert_eq!(parsed, c);
        let text = ListCursor {
            sent_at: None,
            id: "y".into(),
            key: Some("alex".into()),
        };
        let round: ListCursor =
            serde_json::from_str(&serde_json::to_string(&text).unwrap()).unwrap();
        assert_eq!(round, text);
    }

    #[test]
    fn explain_date_ascending_uses_list_index_without_temp_btree() {
        let db = seeded_db();
        let cursor = ListCursor {
            sent_at: Some(1_699_999_000),
            id: format!("{MAILBOX_ID}:500"),
            key: None,
        };
        let pred = segment_a_ascending(Some(&cursor)).expect("cursor in segment A");
        let conn = db.lock_conn();
        for (scope, binds, index) in [
            (
                Scope::Mailbox {
                    account_id: ACCOUNT_ID,
                    mailbox_id: MAILBOX_ID,
                },
                vec![&ACCOUNT_ID as &dyn ToSql, &MAILBOX_ID],
                "idx_messages_list",
            ),
            (
                Scope::Role("inbox"),
                vec![&"inbox" as &dyn ToSql],
                "idx_messages_sent_at",
            ),
        ] {
            let sql = format!(
                "EXPLAIN QUERY PLAN SELECT {HEADER_COLUMNS} {} {} ORDER BY m.sent_at ASC, m.id DESC LIMIT ?",
                scope.header_from(),
                pred.sql
            );
            let mut stmt = conn.prepare(&sql).expect("prepare");
            let mut params = binds;
            for v in &pred.extra {
                params.push(v);
            }
            let limit: i64 = 50;
            params.push(&limit);
            let plan = stmt
                .query_map(params.as_slice(), |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
                .join("\n");
            assert!(plan.contains(index), "expected {index} in plan:\n{plan}");
            assert!(!plan.contains("USE TEMP B-TREE"), "no temp B-tree:\n{plan}");
        }
    }

    /// 100k-row timing; prints the numbers. Run by hand:
    /// `cargo test -p castellum-db --release -- --ignored sorted_page_of_100k --nocapture`
    #[test]
    #[ignore]
    fn sorted_page_of_100k_rows_within_budget() {
        let db = Database::open(&tmp_db_path()).expect("open");
        db.upsert_account(&Account {
            id: "big".into(),
            email: "big@test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.upsert_mailbox(&Mailbox {
            id: "big:INBOX".into(),
            account_id: "big".into(),
            path: "INBOX".into(),
            name: "INBOX".into(),
            role: Some("inbox".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        for i in 0..100_000 {
            db.upsert_message(&text_msg(i, "big:INBOX", "big")).unwrap();
        }
        for sort in [
            ListSort {
                key: SortKey::From,
                ascending: true,
            },
            ListSort {
                key: SortKey::Subject,
                ascending: false,
            },
        ] {
            let t = std::time::Instant::now();
            let page = db
                .list_message_page_sorted("big", "big:INBOX", sort, None, 100)
                .unwrap();
            let first = t.elapsed();
            let t = std::time::Instant::now();
            let _ = db
                .list_message_page_sorted("big", "big:INBOX", sort, page.next.as_ref(), 100)
                .unwrap();
            eprintln!(
                "{sort:?}: first page {first:?}, second page {:?}",
                t.elapsed()
            );
            assert!(
                first.as_secs() < 5,
                "first sorted page of 100k rows took {first:?}"
            );
        }
    }
}
