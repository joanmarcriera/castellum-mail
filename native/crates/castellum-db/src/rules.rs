// Castellum - saved_filters (smart mailboxes + rules) persistence and
// evaluation layer (S40).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// CRUD for `saved_filters` (same shape as `smart_mailboxes.rs`'s CRUD:
// uuid-v4 ids generated on insert if absent, `created_at`/`updated_at`
// timestamps) plus `evaluate_filter` — the ONE function both the ad-hoc/
// dry-run preview FFI entry point and every saved smart-mailbox/rule read
// call into, splicing `filter_sql::compile_group`'s WHERE fragment into the
// same base SELECT shape `list_message_headers` uses (same joins, same
// `row_to_header` projection).
//
// Deviation from the memo's §4 BM25 note: the compiled Subject/Body
// condition is a subquery (`m.id IN (SELECT message_id FROM message_fts
// WHERE message_fts MATCH ?)`, matching the memo's own §4 code sketch) —
// it does not JOIN message_fts into the outer query, so `ORDER BY
// bm25(message_fts)` would reference a table not in scope (invalid SQL).
// Rather than restructure the compiler to join message_fts (ambiguous once
// a group can contain more than one FTS condition, or one nested inside an
// OR), this module always orders by `sort_field`/`sort_desc` — correctness
// (valid SQL, the right match set, never a panic) over ranking. See the
// plan's Deviations log.

use crate::filter_sql::{compile_group, has_fts_condition};
use crate::repo::row_to_header;
use crate::{Database, DbError};
use castellum_models::{
    FilterGroup, MessageHeader, RuleAction, RuleTrigger, SavedFilter, SavedFilterKind,
};
use rusqlite::{params, params_from_iter, types::Value, Connection, OptionalExtension};
use serde::Serialize;
use uuid::Uuid;

/// Unix epoch seconds now — used only for the DB-level upgrade tests below;
/// `saved_filters.created_at`/`updated_at` are `datetime('now')` TEXT
/// columns (memo §5), not epoch integers like `smart_mailboxes`.
fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parameters for [`Database::upsert_filter`]. `id: None` creates (uuid-v4
/// generated); `id: Some(existing)` updates in place.
#[derive(Debug)]
pub struct SavedFilterUpsert<'a> {
    pub id: Option<&'a str>,
    pub account_id: Option<&'a str>,
    pub name: &'a str,
    pub kind: SavedFilterKind,
    pub group: &'a FilterGroup,
    pub sort_field: Option<&'a str>,
    pub sort_desc: bool,
    /// Required when `kind == Rule`, must be `None` when
    /// `kind == SmartMailbox` — the same invariant the table's `CHECK`
    /// enforces; validated here first so the caller gets a readable error
    /// rather than a raw SQLite constraint-violation message.
    pub trigger: Option<RuleTrigger>,
    pub actions: &'a [RuleAction],
}

/// A saved filter's matches paired with the actions that would apply to
/// each of them — the dry-run/preview shape for a `kind = 'rule'` filter
/// specifically (memo's `castellum_evaluate_filter` alone tells you the
/// match set, not the action-per-match mapping; Assumption 4).
#[derive(Debug, Clone, Serialize)]
pub struct RulePreview {
    pub matches: Vec<MessageHeader>,
    pub actions: Vec<RuleAction>,
    /// MM-3 review: the exact number of messages the rule would act on now
    /// (a SQL `COUNT` with the protected-folder exclusion applied), which
    /// `matches` may undercount because it is capped at the caller's limit.
    pub total: i64,
}

/// MM-3 review: mail in these folder roles is never a rule candidate — the
/// user (or another device) already put it there, and acting on it would
/// fight that decision (re-trashing what a Trash rule already trashed,
/// archiving a draft). Applied IN SQL, before any `LIMIT`, so a count or a
/// capped preview can never be skewed by rows filtered out afterwards.
pub const RULE_PROTECTED_ROLES: &[&str] = &["trash", "junk", "drafts", "sent"];

/// SQL form of [`RULE_PROTECTED_ROLES`] for a `messages m` query.
const RULE_PROTECTED_SQL: &str = "m.mailbox_id NOT IN (SELECT id FROM mailboxes \
     WHERE role IN ('trash', 'junk', 'drafts', 'sent'))";

/// Message ids bound per statement when a query is restricted to an id list.
/// SQLite caps host parameters (32766 on current builds, 999 on old ones);
/// a first sync or a long offline gap can hand the rules pass far more ids
/// than that, so every id-list query is chunked well under the old limit.
pub const RULE_ID_CHUNK: usize = 500;

/// How far back [`Database::arrivals_following_own_moves`] looks for moves
/// this app queued. A move normally replays within seconds and the
/// destination copy arrives on the next sync; 90 days covers a Mac that was
/// offline or asleep for a long time without keeping the lookup unbounded.
pub(crate) const OWN_MOVE_LOOKBACK: &str = "-90 days";

/// Stable identity of a message that has no Message-ID: an FNV-1a 64-bit hash
/// of its Date, From, Subject and size. `enqueue_move` stores it in the move
/// op, and [`Database::arrivals_following_own_moves`] recomputes it from the
/// arriving row, so only THE SAME message is recognised as "coming back".
/// A hash (not the headers) because op payloads travel into support tooling.
/// Not cryptographic: a collision only suppresses one rule pass on a
/// different message, the conservative side of the ledger's trade-off.
pub(crate) fn anonymous_message_fingerprint(
    sent_at: Option<i64>,
    from_address: Option<&str>,
    subject: Option<&str>,
    size_bytes: Option<i64>,
) -> String {
    let text = format!(
        "{}\x1f{}\x1f{}\x1f{}",
        sent_at.unwrap_or(0),
        from_address.unwrap_or("").trim().to_lowercase(),
        subject.unwrap_or("").trim().to_lowercase(),
        size_bytes.unwrap_or(0)
    );
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// What a header query is restricted to, beyond the filter group itself.
#[derive(Default)]
struct HeaderScope<'a> {
    account_id: Option<&'a str>,
    message_ids: Option<&'a [String]>,
    /// Leave out mail in [`RULE_PROTECTED_ROLES`] folders.
    rule_candidates_only: bool,
    /// Keyset pagination for Run Now batches: only ids sorting after this.
    after_id: Option<&'a str>,
    /// Count each message once: of the candidate copies that share a
    /// Message-ID within the account (Gmail keeps the same mail in INBOX, in
    /// label folders and in All Mail), keep one, preferring the Inbox copy,
    /// then an ordinary folder, last the archive (All Mail). A message that
    /// exists only as an All Mail copy still has its one copy. Mail without
    /// a Message-ID is never merged with anything.
    dedupe_copies: bool,
}

fn kind_to_str(kind: SavedFilterKind) -> &'static str {
    match kind {
        SavedFilterKind::SmartMailbox => "smart_mailbox",
        SavedFilterKind::Rule => "rule",
    }
}

fn kind_from_str(s: &str) -> Result<SavedFilterKind, DbError> {
    match s {
        "smart_mailbox" => Ok(SavedFilterKind::SmartMailbox),
        "rule" => Ok(SavedFilterKind::Rule),
        other => Err(DbError(format!("unknown saved_filters.kind: {other}"))),
    }
}

fn trigger_to_str(t: RuleTrigger) -> &'static str {
    match t {
        RuleTrigger::OnSync => "on_sync",
        RuleTrigger::Manual => "manual",
    }
}

fn trigger_from_str(s: &str) -> Result<RuleTrigger, DbError> {
    match s {
        "on_sync" => Ok(RuleTrigger::OnSync),
        "manual" => Ok(RuleTrigger::Manual),
        other => Err(DbError(format!(
            "unknown saved_filters.trigger_kind: {other}"
        ))),
    }
}

fn row_to_saved_filter(r: &rusqlite::Row<'_>) -> rusqlite::Result<(SavedFilter, String)> {
    // Returns the raw kind/group/actions strings needing fallible JSON/enum
    // decode alongside the mostly-built struct, so the caller can surface a
    // `DbError` instead of panicking on a corrupt row — see
    // `fetch_saved_filter`.
    let kind_str: String = r.get(3)?;
    let group_json: String = r.get(4)?;
    let trigger_kind: Option<String> = r.get(7)?;
    let actions_json: String = r.get(8)?;
    let filter = SavedFilter {
        id: r.get(0)?,
        account_id: r.get(1)?,
        name: r.get(2)?,
        kind: SavedFilterKind::SmartMailbox, // placeholder, replaced by the caller
        group: FilterGroup {
            op: castellum_models::BoolOp::And,
            conditions: vec![],
            groups: vec![],
        },
        sort_field: r.get(5)?,
        sort_desc: r.get::<_, i64>(6)? != 0,
        trigger: trigger_kind
            .as_deref()
            .map(trigger_from_str)
            .transpose()
            .unwrap_or(None),
        actions: vec![],
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
        sort_order: r.get(11)?,
        stop_processing: r.get::<_, i64>(12)? != 0,
        enabled: r.get::<_, i64>(13)? != 0,
    };
    Ok((
        filter,
        format!("{kind_str}\u{0}{group_json}\u{0}{actions_json}"),
    ))
}

const SELECT_COLUMNS: &str = "id, account_id, name, kind, group_json, sort_field, sort_desc, \
    trigger_kind, actions_json, created_at, updated_at, sort_order, stop_processing, enabled";

/// Decodes the row produced by `row_to_saved_filter`'s packed sidecar string
/// into the final `SavedFilter`, surfacing a `DbError` (never a panic) on a
/// corrupt `kind`/`group_json`/`actions_json`/`trigger_kind`.
fn finish_saved_filter(pair: (SavedFilter, String)) -> Result<SavedFilter, DbError> {
    let (mut filter, packed) = pair;
    let mut parts = packed.splitn(3, '\u{0}');
    let kind_str = parts.next().unwrap_or_default();
    let group_json = parts.next().unwrap_or_default();
    let actions_json = parts.next().unwrap_or_default();

    filter.kind = kind_from_str(kind_str)?;
    filter.group = serde_json::from_str(group_json)
        .map_err(|e| DbError(format!("corrupt saved_filters.group_json: {e}")))?;
    filter.actions = serde_json::from_str(actions_json)
        .map_err(|e| DbError(format!("corrupt saved_filters.actions_json: {e}")))?;
    Ok(filter)
}

fn fetch_saved_filter(conn: &Connection, id: &str) -> Result<SavedFilter, DbError> {
    let pair = conn.query_row(
        &format!("SELECT {SELECT_COLUMNS} FROM saved_filters WHERE id = ?1"),
        params![id],
        row_to_saved_filter,
    )?;
    finish_saved_filter(pair)
}

/// How long a finished `move` op keeps shielding its Message-ID from a Run Now
/// candidate query (see `scope_predicates`).
const RUN_NOW_MOVE_SHIELD: &str = "-1 hour";

impl Database {
    /// Create (`id` absent/empty) or update (`id` present) a saved filter.
    /// Rejects an empty name, and a `kind`/`trigger` mismatch (the same
    /// invariant the table's `CHECK` enforces, validated here first for a
    /// readable error).
    pub fn upsert_filter(&self, u: SavedFilterUpsert<'_>) -> Result<SavedFilter, DbError> {
        self.upsert_filter_with_options(u, None, None)
    }

    /// [`Database::upsert_filter`] plus a rule's processing options
    /// (`None` leaves a field as is; a new row starts not-stopping and
    /// enabled), written in ONE transaction (MM-3 review): a failure between
    /// the two writes can no longer leave a saved rule running with its old
    /// stop/enabled switches.
    pub fn upsert_filter_with_options(
        &self,
        u: SavedFilterUpsert<'_>,
        stop_processing: Option<bool>,
        enabled: Option<bool>,
    ) -> Result<SavedFilter, DbError> {
        let name = u.name.trim();
        if name.is_empty() {
            return Err(DbError("saved filter name must not be empty".to_string()));
        }
        match (u.kind, u.trigger) {
            (SavedFilterKind::SmartMailbox, Some(_)) => {
                return Err(DbError(
                    "a smart_mailbox filter must not carry a trigger".to_string(),
                ))
            }
            (SavedFilterKind::Rule, None) => {
                return Err(DbError("a rule filter must carry a trigger".to_string()))
            }
            _ => {}
        }
        if u.kind == SavedFilterKind::Rule {
            // A rule runs unattended after every sync: refuse, at save, a
            // tree the compiler would reject on every run.
            crate::filter_sql::validate_limits(u.group)?;
            crate::filter_sql::compile_group(u.group, &mut Vec::new())?;
            // MM-3 review: at most one action may take the message out of
            // its folder. The engine could only ever run the first (the row
            // is gone after it), so a second one used to be dropped
            // silently; refuse it here instead, where the user can see why.
            if u.actions.iter().filter(|a| a.removes_message()).count() > 1 {
                return Err(DbError(
                    "a rule can have only one action that moves the message \
                     (Move, Archive or Trash)"
                        .to_string(),
                ));
            }
            // #3146: tag actions name a real, user-visible keyword, and a
            // rule never both adds and removes the same one (the result
            // would depend on action order).
            let mut added = std::collections::HashSet::new();
            let mut removed = std::collections::HashSet::new();
            for a in u.actions {
                let Some(k) = a.tag_keyword() else { continue };
                let k = k.trim();
                if k.is_empty() || k.starts_with('\\') || k.chars().any(|c| c.is_whitespace()) {
                    return Err(DbError(format!(
                        "a rule tag action needs a tag keyword, not '{k}'"
                    )));
                }
                let key = k.to_lowercase();
                if matches!(a, castellum_models::RuleAction::AddTag { .. }) {
                    added.insert(key);
                } else {
                    removed.insert(key);
                }
            }
            if added.intersection(&removed).next().is_some() {
                return Err(DbError(
                    "a rule cannot both add and remove the same tag".to_string(),
                ));
            }
        }
        let group_json = serde_json::to_string(u.group)
            .map_err(|e| DbError(format!("failed to serialize filter group: {e}")))?;
        let actions_json = serde_json::to_string(u.actions)
            .map_err(|e| DbError(format!("failed to serialize rule actions: {e}")))?;
        let kind_str = kind_to_str(u.kind);
        let trigger_str = u.trigger.map(trigger_to_str);
        let now = now_epoch();
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;

        let id = if let Some(id) = u.id.filter(|s| !s.is_empty()) {
            let n = tx.execute(
                "UPDATE saved_filters SET account_id = ?2, name = ?3, kind = ?4, group_json = ?5, \
                 sort_field = ?6, sort_desc = ?7, trigger_kind = ?8, actions_json = ?9, \
                 updated_at = datetime('now') WHERE id = ?1",
                params![
                    id,
                    u.account_id,
                    name,
                    kind_str,
                    group_json,
                    u.sort_field,
                    u.sort_desc as i64,
                    trigger_str,
                    actions_json
                ],
            )?;
            if n == 0 {
                insert_filter(
                    &tx,
                    id,
                    &u,
                    kind_str,
                    &group_json,
                    trigger_str,
                    &actions_json,
                    now,
                )?;
            }
            id.to_string()
        } else {
            let new_id = Uuid::new_v4().to_string();
            insert_filter(
                &tx,
                &new_id,
                &u,
                kind_str,
                &group_json,
                trigger_str,
                &actions_json,
                now,
            )?;
            new_id
        };
        if stop_processing.is_some() || enabled.is_some() {
            tx.execute(
                "UPDATE saved_filters SET stop_processing = COALESCE(?2, stop_processing), \
                 enabled = COALESCE(?3, enabled) WHERE id = ?1",
                params![id, stop_processing.map(i64::from), enabled.map(i64::from)],
            )?;
        }
        let stored = fetch_saved_filter(&tx, &id)?;
        tx.commit()?;
        Ok(stored)
    }

    /// Filters scoped to `account_id` (its own rows plus every
    /// account-agnostic `account_id IS NULL` row — Assumption 7's
    /// "None-means-all-accounts" convention), or every filter when
    /// `account_id` is `None`.
    pub fn list_filters(&self, account_id: Option<&str>) -> Result<Vec<SavedFilter>, DbError> {
        let conn = self.lock_conn();
        let (sql, has_scope) = match account_id {
            Some(_) => (
                format!(
                    "SELECT {SELECT_COLUMNS} FROM saved_filters \
                     WHERE account_id = ?1 OR account_id IS NULL ORDER BY sort_order, name"
                ),
                true,
            ),
            None => (
                format!("SELECT {SELECT_COLUMNS} FROM saved_filters ORDER BY sort_order, name"),
                false,
            ),
        };
        let mut stmt = conn.prepare(&sql)?;
        let pairs = if has_scope {
            stmt.query_map(params![account_id], row_to_saved_filter)?
                .collect::<Result<Vec<_>, _>>()?
        } else {
            stmt.query_map([], row_to_saved_filter)?
                .collect::<Result<Vec<_>, _>>()?
        };
        pairs.into_iter().map(finish_saved_filter).collect()
    }

    /// `None` when `id` is unknown — never an error.
    pub fn get_filter(&self, id: &str) -> Result<Option<SavedFilter>, DbError> {
        let conn = self.lock_conn();
        let pair = conn
            .query_row(
                &format!("SELECT {SELECT_COLUMNS} FROM saved_filters WHERE id = ?1"),
                params![id],
                row_to_saved_filter,
            )
            .optional()?;
        pair.map(finish_saved_filter).transpose()
    }

    /// MM-3: set a rule's processing options. `None` leaves a field as is.
    /// Errors if `id` is unknown.
    pub fn set_rule_options(
        &self,
        id: &str,
        stop_processing: Option<bool>,
        enabled: Option<bool>,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        let n = conn.execute(
            "UPDATE saved_filters SET stop_processing = COALESCE(?2, stop_processing), \
             enabled = COALESCE(?3, enabled), updated_at = datetime('now') WHERE id = ?1",
            params![id, stop_processing.map(i64::from), enabled.map(i64::from)],
        )?;
        if n == 0 {
            return Err(DbError(format!("saved filter {id} not found")));
        }
        Ok(())
    }

    /// MM-3: run order. `sort_order` becomes each id's 0-based position in
    /// `ordered_ids`; ids not listed (e.g. deleted since the caller loaded the
    /// list) are left alone, exactly like `reorder_smart_mailboxes`.
    pub fn reorder_filters(&self, ordered_ids: &[String]) -> Result<(), DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        for (i, id) in ordered_ids.iter().enumerate() {
            tx.execute(
                "UPDATE saved_filters SET sort_order = ?2 WHERE id = ?1",
                params![id, i as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Permanently delete a saved filter. Errors if `id` is unknown
    /// (idempotent-delete-is-an-error, matching `delete_smart_mailbox`).
    pub fn delete_filter(&self, id: &str) -> Result<(), DbError> {
        let conn = self.lock_conn();
        let n = conn.execute("DELETE FROM saved_filters WHERE id = ?1", params![id])?;
        if n == 0 {
            return Err(DbError(format!("saved filter {id} not found")));
        }
        Ok(())
    }

    /// Evaluates `group` and returns matching `MessageHeader` rows — the ONE
    /// function both `castellum_evaluate_filter` (ad-hoc/dry-run preview)
    /// and every saved smart-mailbox/rule read call into. `account_id: None`
    /// searches every account; `Some(id)` scopes to one.
    pub fn evaluate_filter(
        &self,
        group: &FilterGroup,
        account_id: Option<&str>,
        sort_field: Option<&str>,
        sort_desc: bool,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<MessageHeader>, DbError> {
        self.evaluate_filter_restricted(
            group, account_id, None, sort_field, sort_desc, limit, offset,
        )
    }

    /// As `evaluate_filter`, but additionally restricted to `message_ids`
    /// when `Some` (Assumption 3 — the ingest hook batches rule evaluation
    /// over one sync pass's newly-synced message ids rather than a full
    /// mailbox rescan). `pub` (not `pub(crate)`): `castellum_core::rules`
    /// (a separate crate) is the only caller besides this module's own
    /// tests; every other caller goes through `evaluate_filter`.
    #[allow(clippy::too_many_arguments)]
    pub fn evaluate_filter_restricted(
        &self,
        group: &FilterGroup,
        account_id: Option<&str>,
        message_ids: Option<&[String]>,
        sort_field: Option<&str>,
        sort_desc: bool,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<MessageHeader>, DbError> {
        if let Some(ids) = message_ids {
            if ids.is_empty() {
                return Ok(Vec::new());
            }
        }
        let dir = if sort_desc { "DESC" } else { "ASC" };
        // `has_fts_condition` tells us the group contains a ranked FTS
        // subquery, but that subquery never JOINs message_fts into this
        // outer query (see the module doc comment's BM25 deviation note),
        // so `bm25(message_fts)` is not in scope here — an FTS-containing
        // group still falls back to `sort_field`/`sent_at`, same as a
        // purely structured one. Threaded through explicitly (rather than
        // silently ignored) so a future slice that adds the join only needs
        // to change this one branch.
        let order_clause = if has_fts_condition(group) {
            format!("ORDER BY m.sent_at {dir}, m.id")
        } else {
            match sort_field {
                Some("size") => format!("ORDER BY m.size_bytes {dir}, m.id"),
                Some("subject") => format!("ORDER BY m.subject {dir}, m.id"),
                _ => format!("ORDER BY m.sent_at {dir}, m.id"),
            }
        };
        self.query_headers(
            group,
            &HeaderScope {
                account_id,
                message_ids,
                ..HeaderScope::default()
            },
            &order_clause,
            limit,
            offset,
        )
    }

    /// The WHERE predicates for `group` within `scope`, pushing their
    /// parameters onto `params_vec` in order.
    fn scope_predicates(
        group: &FilterGroup,
        scope: &HeaderScope<'_>,
        params_vec: &mut Vec<Value>,
    ) -> Result<Vec<String>, DbError> {
        let where_sql = compile_group(group, params_vec)?;
        // Parenthesise: `compile_group` deliberately leaves its top-level
        // result unwrapped (its own parens would be redundant standing
        // alone — see filter_sql.rs `compile_group`'s doc comment), but
        // this caller ANDs it with `m.account_id = ?` / `m.id IN (...)`
        // scope predicates below. A top-level `OR` group left unwrapped
        // binds looser than that AND (SQLite: `a OR b AND c` parses as
        // `a OR (b AND c)`), which would leak the scope predicates out of
        // one arm of the OR — verified against a real DB before this fix.
        let mut predicates = vec![format!("({where_sql})")];
        if let Some(acc) = scope.account_id {
            predicates.push("m.account_id = ?".to_string());
            params_vec.push(Value::Text(acc.to_string()));
        }
        if let Some(ids) = scope.message_ids {
            let placeholders = vec!["?"; ids.len()].join(", ");
            predicates.push(format!("m.id IN ({placeholders})"));
            for id in ids {
                params_vec.push(Value::Text(id.clone()));
            }
        }
        if scope.rule_candidates_only {
            predicates.push(RULE_PROTECTED_SQL.to_string());
        }
        if let Some(after) = scope.after_id {
            predicates.push("m.id > ?".to_string());
            params_vec.push(Value::Text(after.to_string()));
        }
        if scope.dedupe_copies {
            // The representative is chosen among ALL candidates (so `after_id`
            // is deliberately not applied inside): a batch must not promote a
            // second copy just because the preferred one was already handled.
            // The inner `m` shadows the outer one on purpose.
            let inner = HeaderScope {
                account_id: scope.account_id,
                message_ids: scope.message_ids,
                rule_candidates_only: scope.rule_candidates_only,
                after_id: None,
                dedupe_copies: false,
            };
            let mut inner_params: Vec<Value> = Vec::new();
            let inner_preds = Self::scope_predicates(group, &inner, &mut inner_params)?;
            predicates.push(format!(
                "m.id IN (SELECT d.id FROM (
                    SELECT m.id AS id, ROW_NUMBER() OVER (
                        PARTITION BY m.account_id, COALESCE(NULLIF(m.rfc822_message_id, ''), m.id)
                        ORDER BY CASE dmb.role WHEN 'inbox' THEN 0 WHEN 'archive' THEN 2 ELSE 1 END, m.id
                    ) AS rn
                    FROM messages m JOIN mailboxes dmb ON dmb.id = m.mailbox_id
                    WHERE {}
                 ) d WHERE d.rn = 1)",
                inner_preds.join(" AND ")
            ));
            params_vec.extend(inner_params);
            // A copy of a message this app has just moved is not a fresh
            // candidate: the row-removing action deletes the preferred (Inbox)
            // copy, so the next batch's ranking would promote the Gmail All Mail
            // copy and act on the same logical message twice (bug hunt #49).
            // `enqueue_move` records the Message-ID in the op payload; ops that
            // are still live, or finished within the last hour (a run's batches
            // and its Continue clicks), shield it. A narrow window on purpose:
            // mail the user later puts back must be matchable again.
            predicates.push(format!(
                "NOT EXISTS (SELECT 1 FROM operations mo
                  WHERE mo.account_id = m.account_id AND mo.kind = 'move'
                    AND mo.status <> 'cancelled'
                    AND COALESCE(m.rfc822_message_id, '') <> ''
                    AND json_extract(mo.payload_json, '$.message_id') = m.rfc822_message_id
                    AND (mo.status IN ('queued', 'running', 'failed')
                         OR mo.updated_at >= datetime('now', '{RUN_NOW_MOVE_SHIELD}')))"
            ));
        }
        Ok(predicates)
    }

    /// One header query: `group` within `scope`, ordered and paged.
    fn query_headers(
        &self,
        group: &FilterGroup,
        scope: &HeaderScope<'_>,
        order_clause: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<MessageHeader>, DbError> {
        let mut params_vec: Vec<Value> = Vec::new();
        let predicates = Self::scope_predicates(group, scope, &mut params_vec)?;
        let sql = format!(
            "SELECT m.id, m.account_id, m.mailbox_id, m.subject, m.from_name, m.from_address,
                    m.sent_at, m.snippet, m.has_attachments, COALESCE(f.is_read, 0),
                    COALESCE(t.message_count, 1), COALESCE(f.is_starred, 0),
                    b.message_id IS NOT NULL, COALESCE(t.is_muted, 0), EXISTS(SELECT 1 FROM vip_senders v WHERE v.address_key = lower(trim(m.from_address)))
             FROM messages m
             LEFT JOIN message_flags f ON f.message_id = m.id
             LEFT JOIN threads t ON t.id = m.thread_id
             LEFT JOIN message_bodies b ON b.message_id = m.id
             WHERE {predicate_sql}
             {order_clause}
             LIMIT ? OFFSET ?",
            predicate_sql = predicates.join(" AND "),
        );
        params_vec.push(Value::Integer(limit));
        params_vec.push(Value::Integer(offset));

        let mut headers: Vec<MessageHeader> = {
            let conn = self.lock_conn();
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(params_vec.iter()), row_to_header)?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        self.attach_keywords(&mut headers)?;
        self.attach_recipients(&mut headers)?;
        Ok(headers)
    }

    /// R1: without an id list each message is counted once (see
    /// `HeaderScope::dedupe_copies`: Gmail's INBOX / label / All Mail copies
    /// of one Message-ID collapse to one candidate, the Inbox copy first).
    ///
    /// MM-3 review: the messages a rule may act on — `group` within
    /// `account_id`, never in a [`RULE_PROTECTED_ROLES`] folder — in message
    /// id order, at most `limit` of them, starting after `after_id` (keyset
    /// pagination: Run Now walks a large match set in bounded batches, and a
    /// batch whose actions do not change the match set, e.g. Mark as read,
    /// never sees the same message twice). `message_ids` restricts the set
    /// to those ids (the arrival pass); it is bound [`RULE_ID_CHUNK`] ids per
    /// statement, so any number of ids is safe.
    pub fn rule_candidates(
        &self,
        group: &FilterGroup,
        account_id: Option<&str>,
        message_ids: Option<&[String]>,
        after_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<MessageHeader>, DbError> {
        let order = "ORDER BY m.id ASC";
        let Some(ids) = message_ids else {
            let scope = HeaderScope {
                account_id,
                rule_candidates_only: true,
                after_id,
                dedupe_copies: true,
                ..HeaderScope::default()
            };
            return self.query_headers(group, &scope, order, limit, 0);
        };
        // Sorted before chunking, so chunk results concatenate in id order.
        let mut ids: Vec<String> = ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let mut out = Vec::new();
        for chunk in ids.chunks(RULE_ID_CHUNK) {
            let left = limit.saturating_sub(out.len() as i64);
            if left <= 0 {
                break;
            }
            let scope = HeaderScope {
                account_id,
                message_ids: Some(chunk),
                rule_candidates_only: true,
                after_id,
                dedupe_copies: false,
            };
            out.extend(self.query_headers(group, &scope, order, left, 0)?);
        }
        Ok(out)
    }

    /// MM-3 review: exact number of [`Database::rule_candidates`] for
    /// `group` (no id list, no limit) — what Run Now's confirmation and the
    /// editor's dry run show, counted in SQL rather than by fetching rows.
    pub fn count_rule_candidates(
        &self,
        group: &FilterGroup,
        account_id: Option<&str>,
    ) -> Result<i64, DbError> {
        self.count_rule_candidates_after(group, account_id, None)
    }

    /// As [`Database::count_rule_candidates`], but only candidates whose id
    /// sorts after `after_id` — what a Run Now batch still has ahead of it.
    pub fn count_rule_candidates_after(
        &self,
        group: &FilterGroup,
        account_id: Option<&str>,
        after_id: Option<&str>,
    ) -> Result<i64, DbError> {
        let mut params_vec: Vec<Value> = Vec::new();
        let scope = HeaderScope {
            account_id,
            rule_candidates_only: true,
            after_id,
            dedupe_copies: true,
            ..HeaderScope::default()
        };
        let predicates = Self::scope_predicates(group, &scope, &mut params_vec)?;
        let sql = format!(
            "SELECT COUNT(*) FROM messages m WHERE {}",
            predicates.join(" AND ")
        );
        let conn = self.lock_conn();
        Ok(conn.query_row(&sql, params_from_iter(params_vec.iter()), |r| r.get(0))?)
    }

    /// MM-3 review (arrival rules must never undo a move): which of
    /// `message_ids` (new rows in `account_id`) are a message THIS app moved
    /// recently — so their arrival is that message coming back, not new
    /// mail. `enqueue_move` deletes the source row at once and replay never
    /// inserts the destination row, so a moved message shows up as a
    /// brand-new row on a later sync wherever it lands: a user's "move back
    /// to Inbox", a restore from Trash (in this app or on another device,
    /// after a rule trashed it), or another rule's Move. Without this, a
    /// Trash rule re-trashed restored mail and two Move rules could
    /// ping-pong a message forever.
    ///
    /// The ledger is the `operations` table itself (`move` ops are kept
    /// after they succeed): an arrival matches when a non-cancelled move op
    /// from the last 90 days names its RFC 822 Message-ID, whatever its
    /// destination. A message with no Message-ID matches only a move whose
    /// stored fingerprint (hash of Date, From, Subject and size) equals its
    /// own, so one such move no longer shields every later Message-ID-less
    /// Inbox arrival. Moves queued before fingerprints existed keep the old
    /// destination-folder match until they leave the 90-day window.
    /// Deliberately conservative — skipping a rule for
    /// a duplicate copy of a message we already moved is harmless, while
    /// re-applying one to mail the user just put back is not.
    pub fn arrivals_following_own_moves(
        &self,
        account_id: &str,
        message_ids: &[String],
    ) -> Result<std::collections::HashSet<String>, DbError> {
        use std::collections::HashSet;
        let mut out = HashSet::new();
        if message_ids.is_empty() {
            return Ok(out);
        }
        let conn = self.lock_conn();
        // (Message-IDs we moved; fingerprints of Message-ID-less moves; and
        // destination paths of legacy Message-ID-less moves that predate
        // fingerprints).
        let mut moved_ids: HashSet<String> = HashSet::new();
        let mut anonymous_fps: HashSet<String> = HashSet::new();
        let mut anonymous_dests: HashSet<String> = HashSet::new();
        {
            let mut stmt = conn.prepare(
                "SELECT COALESCE(json_extract(payload_json, '$.message_id'), ''),
                        COALESCE(json_extract(payload_json, '$.dest_path'), ''),
                        COALESCE(json_extract(payload_json, '$.fingerprint'), ''),
                        kind
                   FROM operations
                  WHERE account_id = ?1 AND kind IN ('move', 'undo_move')
                    AND status <> 'cancelled'
                    AND created_at >= datetime('now', ?2)",
            )?;
            let rows = stmt.query_map(params![account_id, OWN_MOVE_LOOKBACK], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?;
            for row in rows {
                let (rfc, dest, fp, kind) = row?;
                if !rfc.is_empty() {
                    moved_ids.insert(rfc);
                } else if !fp.is_empty() {
                    anonymous_fps.insert(fp);
                } else if kind == "move" {
                    // An `undo_move` names the INBOX as its destination; counting
                    // that path would silence arrival rules for every future
                    // Message-ID-less Inbox arrival (undo guard G3).
                    anonymous_dests.insert(dest);
                }
            }
        }
        if moved_ids.is_empty() && anonymous_fps.is_empty() && anonymous_dests.is_empty() {
            return Ok(out);
        }
        for chunk in message_ids.chunks(RULE_ID_CHUNK) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                "SELECT m.id, mb.path, COALESCE(m.rfc822_message_id, ''),
                        m.sent_at, m.from_address, m.subject, m.size_bytes
                   FROM messages m JOIN mailboxes mb ON mb.id = m.mailbox_id
                  WHERE m.id IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(chunk.iter()), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    anonymous_message_fingerprint(
                        r.get::<_, Option<i64>>(3)?,
                        r.get::<_, Option<String>>(4)?.as_deref(),
                        r.get::<_, Option<String>>(5)?.as_deref(),
                        r.get::<_, Option<i64>>(6)?,
                    ),
                ))
            })?;
            for row in rows {
                let (id, path, rfc, fp) = row?;
                let ours = if rfc.is_empty() {
                    anonymous_fps.contains(&fp) || anonymous_dests.contains(&path)
                } else {
                    moved_ids.contains(&rfc)
                };
                if ours {
                    out.insert(id);
                }
            }
        }
        Ok(out)
    }

    /// Loads the saved `rule`-kind filter `filter_id`, evaluates its group,
    /// and returns the matches paired with the filter's own `actions` list
    /// (same actions for every match — the schema has no per-condition
    /// action mapping) — the dry-run/preview primitive for a rule
    /// specifically. Errors (never a panic, never a silent empty success) if
    /// `filter_id` is unknown or is not `kind = 'rule'`.
    pub fn preview_rule(
        &self,
        filter_id: &str,
        account_id: Option<&str>,
        limit: i64,
    ) -> Result<RulePreview, DbError> {
        let filter = self
            .get_filter(filter_id)?
            .ok_or_else(|| DbError(format!("filter {filter_id} not found")))?;
        if filter.kind != SavedFilterKind::Rule {
            return Err(DbError(format!(
                "filter {filter_id} is not a rule (kind = {:?}) — preview_rule only applies to rules",
                filter.kind
            )));
        }
        let scope = account_id.or(filter.account_id.as_deref());
        let (matches, total) = self.preview_rule_group(
            &filter.group,
            scope,
            filter.sort_field.as_deref(),
            filter.sort_desc,
            limit,
        )?;
        Ok(RulePreview {
            matches,
            actions: filter.actions,
            total,
        })
    }

    /// The dry run for any rule condition group, saved or not: the first
    /// `limit` messages a rule with `group` would act on (protected folders
    /// excluded in SQL, before the limit) and the exact total.
    pub fn preview_rule_group(
        &self,
        group: &FilterGroup,
        account_id: Option<&str>,
        sort_field: Option<&str>,
        sort_desc: bool,
        limit: i64,
    ) -> Result<(Vec<MessageHeader>, i64), DbError> {
        let dir = if sort_desc { "DESC" } else { "ASC" };
        let order_clause = match sort_field {
            Some("size") if !has_fts_condition(group) => {
                format!("ORDER BY m.size_bytes {dir}, m.id")
            }
            Some("subject") if !has_fts_condition(group) => {
                format!("ORDER BY m.subject {dir}, m.id")
            }
            _ => format!("ORDER BY m.sent_at {dir}, m.id"),
        };
        let scope = HeaderScope {
            account_id,
            rule_candidates_only: true,
            dedupe_copies: true,
            ..HeaderScope::default()
        };
        let matches = self.query_headers(group, &scope, &order_clause, limit, 0)?;
        let total = self.count_rule_candidates(group, account_id)?;
        Ok((matches, total))
    }
}

#[allow(clippy::too_many_arguments)]
fn insert_filter(
    conn: &Connection,
    id: &str,
    u: &SavedFilterUpsert<'_>,
    kind_str: &str,
    group_json: &str,
    trigger_str: Option<&str>,
    actions_json: &str,
    _now: i64,
) -> Result<(), DbError> {
    conn.execute(
        "INSERT INTO saved_filters
            (id, account_id, name, kind, group_json, sort_field, sort_desc, trigger_kind, actions_json,
             sort_order)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                 (SELECT COALESCE(MAX(sort_order), -1) + 1 FROM saved_filters))",
        params![
            id, u.account_id, u.name.trim(), kind_str, group_json, u.sort_field,
            u.sort_desc as i64, trigger_str, actions_json
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use castellum_models::{
        Account, Address, BoolOp, FilterCondition, FilterField, FilterOperator, FilterValue,
        Mailbox, NewMessage,
    };
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_db() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        castellum_testutil::scratch_root().join(format!(
            "castellum-rules-db-test-{}-{}-{n}.sqlite",
            std::process::id(),
            now_epoch()
        ))
    }

    fn seeded_db() -> (Database, PathBuf) {
        let path = tmp_db();
        let db = Database::open(&path).unwrap();
        db.upsert_account(&Account {
            id: "a1".into(),
            email: "me@x.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.upsert_mailbox(&Mailbox {
            id: "a1:INBOX".into(),
            account_id: "a1".into(),
            path: "INBOX".into(),
            name: "INBOX".into(),
            role: Some("inbox".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        (db, path)
    }

    fn seed_message(
        db: &Database,
        id: &str,
        subject: &str,
        from: &str,
        sent_at: i64,
        is_read: bool,
    ) {
        // provider_uid must be distinct within (account_id, mailbox_id) —
        // messages' UNIQUE constraint — so derive it from the message id's
        // own trailing UID rather than a constant.
        let uid: i64 = id
            .rsplit(':')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);
        db.upsert_message(&NewMessage {
            id: id.to_string(),
            account_id: "a1".into(),
            mailbox_id: "a1:INBOX".into(),
            provider_uid: Some(uid),
            rfc822_message_id: Some(format!("<{id}@x.test>")),
            subject: Some(subject.to_string()),
            from_name: None,
            from_address: Some(from.to_string()),
            sent_at: Some(sent_at),
            snippet: None,
            size_bytes: None,
            has_attachments: false,
            attachments: None,
            is_read,
            is_starred: false,
            addresses: vec![Address {
                kind: "from".into(),
                name: None,
                address: from.to_string(),
            }],
            text_body: None,
            html_body: None,
            sanitized_html: None,
            has_remote_content: false,
            references: vec![],
        })
        .unwrap();
    }

    /// As `seed_message`, but lets the caller pick account/mailbox and the
    /// starred/attachment flags — needed by the top-level-OR scope-leak
    /// regression test below, which requires a second account.
    #[allow(clippy::too_many_arguments)]
    fn seed_message_full(
        db: &Database,
        account_id: &str,
        mailbox_id: &str,
        id: &str,
        subject: &str,
        from: &str,
        sent_at: i64,
        is_starred: bool,
        has_attachments: bool,
    ) {
        let uid: i64 = id
            .rsplit(':')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);
        db.upsert_message(&NewMessage {
            id: id.to_string(),
            account_id: account_id.to_string(),
            mailbox_id: mailbox_id.to_string(),
            provider_uid: Some(uid),
            rfc822_message_id: Some(format!("<{id}@x.test>")),
            subject: Some(subject.to_string()),
            from_name: None,
            from_address: Some(from.to_string()),
            sent_at: Some(sent_at),
            snippet: None,
            size_bytes: None,
            has_attachments,
            attachments: None,
            is_read: false,
            is_starred,
            addresses: vec![Address {
                kind: "from".into(),
                name: None,
                address: from.to_string(),
            }],
            text_body: None,
            html_body: None,
            sanitized_html: None,
            has_remote_content: false,
            references: vec![],
        })
        .unwrap();
    }

    fn cleanup(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
    }

    fn group_unread_last_7_days(now: i64) -> FilterGroup {
        FilterGroup {
            op: BoolOp::And,
            conditions: vec![
                FilterCondition {
                    field: FilterField::IsRead,
                    operator: FilterOperator::Is,
                    value: FilterValue::Bool(false),
                },
                FilterCondition {
                    field: FilterField::Date,
                    operator: FilterOperator::After,
                    value: FilterValue::Number(now - 7 * 86_400),
                },
            ],
            groups: vec![],
        }
    }

    fn group_invoices() -> FilterGroup {
        FilterGroup {
            op: BoolOp::And,
            conditions: vec![FilterCondition {
                field: FilterField::Subject,
                operator: FilterOperator::Contains,
                value: FilterValue::Text("invoice".into()),
            }],
            groups: vec![],
        }
    }

    /// Stores one row with an explicit (or no) Message-ID.
    fn seed_rfc(db: &Database, mailbox_id: &str, id: &str, uid: i64, rfc: Option<&str>) {
        seed_rfc_subject(db, mailbox_id, id, uid, rfc, "s");
    }

    /// As `seed_rfc`, with a chosen subject.
    fn seed_rfc_subject(
        db: &Database,
        mailbox_id: &str,
        id: &str,
        uid: i64,
        rfc: Option<&str>,
        subject: &str,
    ) {
        db.upsert_message(&NewMessage {
            id: id.to_string(),
            account_id: "a1".into(),
            mailbox_id: mailbox_id.to_string(),
            provider_uid: Some(uid),
            rfc822_message_id: rfc.map(str::to_string),
            subject: Some(subject.to_string()),
            from_name: None,
            from_address: Some("a@x.test".into()),
            sent_at: Some(1),
            snippet: None,
            size_bytes: None,
            has_attachments: false,
            attachments: None,
            is_read: false,
            is_starred: false,
            addresses: vec![],
            text_body: None,
            html_body: None,
            sanitized_html: None,
            has_remote_content: false,
            references: vec![],
        })
        .unwrap();
    }

    #[test]
    fn arrivals_following_own_moves_match_by_message_id_or_by_folder_without_one() {
        let (db, path) = seeded_db();
        db.set_account_write_enabled("a1", true).unwrap();
        db.upsert_mailbox(&Mailbox {
            id: "a1:Work".into(),
            account_id: "a1".into(),
            path: "Work".into(),
            name: "Work".into(),
            role: None,
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        // Two moves by this app: one message with a Message-ID (to Work),
        // one without (to INBOX).
        seed_rfc(&db, "a1:INBOX", "a1:INBOX:1", 1, Some("<moved@x.test>"));
        seed_rfc(&db, "a1:Work", "a1:Work:2", 2, None);
        db.move_message_to_mailbox("a1:INBOX:1", "a1:Work").unwrap();
        db.move_message_to_mailbox("a1:Work:2", "a1:INBOX").unwrap();
        // The next sync's rows: both come back, plus two genuinely new ones.
        seed_rfc(&db, "a1:INBOX", "a1:INBOX:10", 10, Some("<moved@x.test>"));
        seed_rfc(&db, "a1:INBOX", "a1:INBOX:11", 11, None);
        seed_rfc(&db, "a1:INBOX", "a1:INBOX:12", 12, Some("<fresh@x.test>"));
        seed_rfc_subject(&db, "a1:Work", "a1:Work:13", 13, None, "other");
        let ids: Vec<String> = ["a1:INBOX:10", "a1:INBOX:11", "a1:INBOX:12", "a1:Work:13"]
            .map(String::from)
            .to_vec();
        let ours = db.arrivals_following_own_moves("a1", &ids).unwrap();
        let mut ours: Vec<&str> = ours.iter().map(String::as_str).collect();
        ours.sort_unstable();
        // By Message-ID wherever it lands; without one, only the same message
        // (matching fingerprint), not any other anonymous mail.
        assert_eq!(ours, vec!["a1:INBOX:10", "a1:INBOX:11"]);
        cleanup(&path);
    }

    /// Undo guard G3: an `undo_move` shields its Message-ID like a `move`, but
    /// its destination (the Inbox) never silences Message-ID-less arrivals.
    #[test]
    fn arrivals_following_own_moves_counts_undo_move() {
        let (db, path) = seeded_db();
        seed_rfc(&db, "a1:INBOX", "a1:INBOX:10", 10, Some("<back@x.test>"));
        seed_rfc(&db, "a1:INBOX", "a1:INBOX:11", 11, None);
        {
            let conn = db.lock_conn();
            for payload in [
                r#"{"src_path":"Trash","dest_path":"INBOX","uid":1,"message_id":"<back@x.test>","fingerprint":""}"#,
                r#"{"src_path":"Trash","dest_path":"INBOX","uid":2,"message_id":"","fingerprint":""}"#,
            ] {
                conn.execute(
                    "INSERT INTO operations (account_id, kind, payload_json) VALUES ('a1','undo_move',?1)",
                    params![payload],
                )
                .unwrap();
            }
        }
        let ids = vec!["a1:INBOX:10".to_string(), "a1:INBOX:11".to_string()];
        let ours = db.arrivals_following_own_moves("a1", &ids).unwrap();
        assert!(ours.contains("a1:INBOX:10"), "shielded by Message-ID");
        assert!(
            !ours.contains("a1:INBOX:11"),
            "an anonymous undo_move must not silence every Message-ID-less Inbox arrival"
        );
        cleanup(&path);
    }

    fn add_gmail_folders(db: &Database) {
        for (id, path, role) in [
            ("a1:Label", "Receipts", None),
            ("a1:AllMail", "[Gmail]/All Mail", Some("archive")),
        ] {
            db.upsert_mailbox(&Mailbox {
                id: id.into(),
                account_id: "a1".into(),
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
    }

    fn seed_invoice_copy(db: &Database, mailbox_id: &str, id: &str, uid: i64, rfc: &str) {
        seed_rfc_subject(db, mailbox_id, id, uid, Some(rfc), "invoice");
    }

    /// R2: the dedupe window partitions by (account_id, rfc822_message_id);
    /// `idx_messages_account_rfc822` (migration 0051) must serve that scan
    /// instead of the planner falling back to a full table scan plus sort.
    #[test]
    fn dedupe_candidate_query_uses_the_account_rfc822_index() {
        let (db, path) = seeded_db();
        let group = group_invoices();
        let mut params: Vec<Value> = Vec::new();
        let scope = HeaderScope {
            account_id: Some("a1"),
            rule_candidates_only: true,
            dedupe_copies: true,
            ..HeaderScope::default()
        };
        let predicates = Database::scope_predicates(&group, &scope, &mut params).unwrap();
        let sql = format!(
            "EXPLAIN QUERY PLAN SELECT m.id FROM messages m WHERE {}",
            predicates.join(" AND ")
        );
        let conn = db.lock_conn();
        let mut stmt = conn.prepare(&sql).unwrap();
        let plan: Vec<String> = stmt
            .query_map(params_from_iter(params.iter()), |r| r.get::<_, String>(3))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        drop(stmt);
        drop(conn);
        assert!(
            plan.iter()
                .any(|d| d.contains("idx_messages_account_rfc822")),
            "dedupe query does not use the index: {plan:#?}"
        );
        cleanup(&path);
    }

    /// R2 measurement (manual): one Run Now batch (`rule_candidates`, dedupe
    /// on, `RUN_NOW_BATCH_MAX`-sized) over 120k seeded messages, with and
    /// without migration 0051's index. Each message has a ~25% chance of
    /// sharing its Message-ID with a neighbour (Gmail-style label copies).
    ///
    ///   cargo test -p castellum-db --release rules::tests::bench_dedupe_batch \
    ///       -- --ignored --nocapture
    #[test]
    #[ignore = "manual benchmark: seeds 120k rows"]
    fn bench_dedupe_batch_with_and_without_the_index() {
        let (db, path) = seeded_db();
        {
            let conn = db.lock_conn();
            conn.execute_batch(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 120000)
                 INSERT INTO messages (id, account_id, mailbox_id, provider_uid,
                                       rfc822_message_id, subject, sent_at)
                 SELECT printf('a1:INBOX:%07d', i), 'a1', 'a1:INBOX', i,
                        printf('<m%d@x.test>', CASE WHEN i % 4 = 0 THEN i - 1 ELSE i END),
                        'invoice', 1000000 + i
                 FROM n;
                 ANALYZE;",
            )
            .unwrap();
        }
        let group = group_invoices();
        let time_batch = |db: &Database| {
            let t = std::time::Instant::now();
            let rows = db
                .rule_candidates(&group, Some("a1"), None, None, 1_000)
                .unwrap();
            (t.elapsed(), rows.len())
        };
        let (after, n_after) = time_batch(&db);
        db.lock_conn()
            .execute_batch("DROP INDEX idx_messages_account_rfc822; ANALYZE;")
            .unwrap();
        let (before, n_before) = time_batch(&db);
        assert_eq!(n_before, n_after, "the index must not change the matches");
        println!("R2 batch of {n_after}: before (no index) {before:?}, after (index) {after:?}");
        cleanup(&path);
    }

    #[test]
    fn gmail_copies_of_one_message_are_counted_and_returned_once() {
        // R1 (b): the same Message-ID in INBOX, a label folder and All Mail
        // is one message. A message that only exists in All Mail still is.
        let (db, path) = seeded_db();
        add_gmail_folders(&db);
        // Ids sort so the All Mail copy comes first: the Inbox copy must
        // still be the one kept.
        seed_invoice_copy(&db, "a1:AllMail", "a1:A:1", 1, "<both@x.test>");
        seed_invoice_copy(&db, "a1:Label", "a1:B:1", 1, "<both@x.test>");
        seed_invoice_copy(&db, "a1:INBOX", "a1:C:1", 1, "<both@x.test>");
        seed_invoice_copy(&db, "a1:AllMail", "a1:A:2", 2, "<archived-only@x.test>");
        let group = group_invoices();
        assert_eq!(db.count_rule_candidates(&group, Some("a1")).unwrap(), 2);
        let ids: Vec<String> = db
            .rule_candidates(&group, Some("a1"), None, None, 100)
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect();
        assert_eq!(ids, vec!["a1:A:2", "a1:C:1"]);
        let (preview, total) = db
            .preview_rule_group(&group, Some("a1"), None, true, 100)
            .unwrap();
        assert_eq!((preview.len(), total), (2, 2));
        // Paging past the kept copy must not promote a sibling copy.
        let rest = db
            .rule_candidates(&group, Some("a1"), None, Some("a1:C:1"), 100)
            .unwrap();
        assert!(rest.is_empty(), "{rest:?}");
        assert_eq!(
            db.count_rule_candidates_after(&group, Some("a1"), Some("a1:C:1"))
                .unwrap(),
            0
        );
        cleanup(&path);
    }

    #[test]
    fn mail_without_a_message_id_is_never_merged_as_a_copy() {
        let (db, path) = seeded_db();
        add_gmail_folders(&db);
        seed_rfc_subject(&db, "a1:INBOX", "a1:INBOX:1", 1, None, "invoice");
        seed_rfc_subject(&db, "a1:AllMail", "a1:AllMail:2", 2, None, "invoice");
        assert_eq!(
            db.count_rule_candidates(&group_invoices(), Some("a1"))
                .unwrap(),
            2
        );
        cleanup(&path);
    }

    #[test]
    fn a_second_different_message_without_a_message_id_is_not_suppressed() {
        // R1 (c): one anonymous move into INBOX used to shield every later
        // anonymous Inbox arrival for 90 days.
        let (db, path) = seeded_db();
        db.set_account_write_enabled("a1", true).unwrap();
        db.upsert_mailbox(&Mailbox {
            id: "a1:Work".into(),
            account_id: "a1".into(),
            path: "Work".into(),
            name: "Work".into(),
            role: None,
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        seed_rfc_subject(&db, "a1:Work", "a1:Work:2", 2, None, "restored mail");
        db.move_message_to_mailbox("a1:Work:2", "a1:INBOX").unwrap();
        // The same message comes back; a different one arrives beside it.
        seed_rfc_subject(&db, "a1:INBOX", "a1:INBOX:10", 10, None, "restored mail");
        seed_rfc_subject(&db, "a1:INBOX", "a1:INBOX:11", 11, None, "brand new mail");
        let ids: Vec<String> = ["a1:INBOX:10", "a1:INBOX:11"].map(String::from).to_vec();
        let ours = db.arrivals_following_own_moves("a1", &ids).unwrap();
        assert!(
            ours.contains("a1:INBOX:10"),
            "the same message stays suppressed"
        );
        assert!(
            !ours.contains("a1:INBOX:11"),
            "a different anonymous arrival gets its rules"
        );
        cleanup(&path);
    }

    #[test]
    fn rule_candidates_leave_out_protected_folders_and_page_by_id() {
        let (db, path) = seeded_db();
        db.upsert_mailbox(&Mailbox {
            id: "a1:Sent".into(),
            account_id: "a1".into(),
            path: "Sent".into(),
            name: "Sent".into(),
            role: Some("sent".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        for n in 1..=3 {
            seed_message(
                &db,
                &format!("a1:INBOX:{n}"),
                "invoice",
                "a@x.test",
                n,
                false,
            );
        }
        seed_message_full(
            &db,
            "a1",
            "a1:Sent",
            "a1:Sent:9",
            "invoice",
            "a@x.test",
            9,
            false,
            false,
        );
        let g = group_invoices();
        assert_eq!(db.count_rule_candidates(&g, Some("a1")).unwrap(), 3);
        let first = db.rule_candidates(&g, Some("a1"), None, None, 2).unwrap();
        let ids: Vec<&str> = first.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["a1:INBOX:1", "a1:INBOX:2"]);
        let rest = db
            .rule_candidates(&g, Some("a1"), None, Some("a1:INBOX:2"), 2)
            .unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].id, "a1:INBOX:3");
        cleanup(&path);
    }

    #[test]
    fn upsert_list_get_delete_round_trip_for_a_smart_mailbox() {
        let (db, path) = seeded_db();
        let group = group_invoices();
        let created = db
            .upsert_filter(SavedFilterUpsert {
                id: None,
                account_id: Some("a1"),
                name: "Invoices",
                kind: SavedFilterKind::SmartMailbox,
                group: &group,
                sort_field: Some("date"),
                sort_desc: true,
                trigger: None,
                actions: &[],
            })
            .unwrap();
        assert_eq!(created.name, "Invoices");
        assert_eq!(created.kind, SavedFilterKind::SmartMailbox);
        assert!(created.trigger.is_none());

        let fetched = db.get_filter(&created.id).unwrap().unwrap();
        assert_eq!(fetched.group, group);

        let listed = db.list_filters(Some("a1")).unwrap();
        assert_eq!(listed.len(), 1);

        db.delete_filter(&created.id).unwrap();
        assert!(db.get_filter(&created.id).unwrap().is_none());
        assert!(
            db.delete_filter(&created.id).is_err(),
            "double delete errors"
        );
        cleanup(&path);
    }

    #[test]
    fn upsert_rejects_kind_trigger_mismatch() {
        let (db, path) = seeded_db();
        let group = group_invoices();
        let err = db
            .upsert_filter(SavedFilterUpsert {
                id: None,
                account_id: None,
                name: "Bad rule",
                kind: SavedFilterKind::Rule,
                group: &group,
                sort_field: None,
                sort_desc: true,
                trigger: None,
                actions: &[],
            })
            .unwrap_err();
        assert!(err.0.contains("must carry a trigger"));
        cleanup(&path);
    }

    #[test]
    fn check_constraint_rejects_kind_trigger_mismatch_at_the_sql_level() {
        // Bypasses the Rust-level validation entirely — pins the table's own
        // CHECK constraint (memo's Test Plan item 5), not just the Rust
        // guard above it.
        let (db, path) = seeded_db();
        let conn = db.lock_conn();
        // smart_mailbox with a non-NULL trigger_kind.
        let bad1 = conn.execute(
            "INSERT INTO saved_filters (id, name, kind, group_json, trigger_kind) \
             VALUES ('bad1', 'x', 'smart_mailbox', '{}', 'on_sync')",
            [],
        );
        assert!(
            bad1.is_err(),
            "CHECK must reject smart_mailbox with a trigger_kind"
        );
        // rule with a NULL trigger_kind.
        let bad2 = conn.execute(
            "INSERT INTO saved_filters (id, name, kind, group_json) \
             VALUES ('bad2', 'x', 'rule', '{}')",
            [],
        );
        assert!(bad2.is_err(), "CHECK must reject rule with no trigger_kind");
        drop(conn);
        cleanup(&path);
    }

    #[test]
    fn evaluate_filter_unread_in_last_7_days_matches_expected_ids() {
        let (db, path) = seeded_db();
        let now = 1_800_000_000i64;
        seed_message(
            &db,
            "a1:INBOX:1",
            "Recent unread",
            "a@x.test",
            now - 1_000,
            false,
        );
        seed_message(
            &db,
            "a1:INBOX:2",
            "Recent read",
            "b@x.test",
            now - 1_000,
            true,
        );
        seed_message(
            &db,
            "a1:INBOX:3",
            "Old unread",
            "c@x.test",
            now - 30 * 86_400,
            false,
        );

        let matches = db
            .evaluate_filter(
                &group_unread_last_7_days(now),
                Some("a1"),
                None,
                true,
                50,
                0,
            )
            .unwrap();
        let ids: Vec<&str> = matches.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["a1:INBOX:1"]);
        cleanup(&path);
    }

    #[test]
    fn evaluate_filter_invoices_matches_expected_ids() {
        let (db, path) = seeded_db();
        seed_message(
            &db,
            "a1:INBOX:1",
            "Your invoice is ready",
            "billing@x.test",
            1,
            false,
        );
        seed_message(&db, "a1:INBOX:2", "Hello there", "a@x.test", 2, false);

        let matches = db
            .evaluate_filter(&group_invoices(), Some("a1"), None, true, 50, 0)
            .unwrap();
        let ids: Vec<&str> = matches.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["a1:INBOX:1"]);
        cleanup(&path);
    }

    #[test]
    fn evaluate_filter_ands_fts_and_flag_condition_intersection_not_union() {
        let (db, path) = seeded_db();
        // Matches subject but is read -> excluded.
        seed_message(&db, "a1:INBOX:1", "invoice due", "a@x.test", 1, true);
        // Matches subject AND unread -> included.
        seed_message(&db, "a1:INBOX:2", "invoice overdue", "b@x.test", 2, false);
        // Unread but no subject match -> excluded.
        seed_message(&db, "a1:INBOX:3", "hello", "c@x.test", 3, false);

        let group = FilterGroup {
            op: BoolOp::And,
            conditions: vec![
                FilterCondition {
                    field: FilterField::Subject,
                    operator: FilterOperator::Contains,
                    value: FilterValue::Text("invoice".into()),
                },
                FilterCondition {
                    field: FilterField::IsRead,
                    operator: FilterOperator::Is,
                    value: FilterValue::Bool(false),
                },
            ],
            groups: vec![],
        };
        let matches = db
            .evaluate_filter(&group, Some("a1"), None, true, 50, 0)
            .unwrap();
        let ids: Vec<&str> = matches.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["a1:INBOX:2"],
            "AND must intersect, never accidentally union"
        );
        cleanup(&path);
    }

    #[test]
    fn preview_rule_pairs_matches_with_the_rules_own_actions() {
        let (db, path) = seeded_db();
        seed_message(&db, "a1:INBOX:1", "invoice due", "a@x.test", 1, false);
        let rule = db
            .upsert_filter(SavedFilterUpsert {
                id: None,
                account_id: Some("a1"),
                name: "Invoice rule",
                kind: SavedFilterKind::Rule,
                group: &group_invoices(),
                sort_field: None,
                sort_desc: true,
                trigger: Some(RuleTrigger::OnSync),
                actions: &[RuleAction::MarkRead],
            })
            .unwrap();

        let preview = db.preview_rule(&rule.id, None, 50).unwrap();
        assert_eq!(preview.matches.len(), 1);
        assert_eq!(preview.matches[0].id, "a1:INBOX:1");
        assert_eq!(preview.actions, vec![RuleAction::MarkRead]);
        cleanup(&path);
    }

    #[test]
    fn preview_rule_against_a_smart_mailbox_errors() {
        let (db, path) = seeded_db();
        let sm = db
            .upsert_filter(SavedFilterUpsert {
                id: None,
                account_id: Some("a1"),
                name: "Just a mailbox",
                kind: SavedFilterKind::SmartMailbox,
                group: &group_invoices(),
                sort_field: None,
                sort_desc: true,
                trigger: None,
                actions: &[],
            })
            .unwrap();
        let err = db.preview_rule(&sm.id, None, 50).unwrap_err();
        assert!(err.0.contains("not a rule"));
        cleanup(&path);
    }

    #[test]
    fn preview_rule_against_unknown_id_errors_not_panics() {
        let (db, path) = seeded_db();
        let err = db.preview_rule("nope", None, 50).unwrap_err();
        assert!(err.0.contains("not found"));
        cleanup(&path);
    }

    #[test]
    fn evaluate_filter_restricted_to_message_ids_only_matches_those() {
        let (db, path) = seeded_db();
        seed_message(&db, "a1:INBOX:1", "invoice due", "a@x.test", 1, false);
        seed_message(&db, "a1:INBOX:2", "invoice also", "b@x.test", 2, false);
        let restricted = db
            .evaluate_filter_restricted(
                &group_invoices(),
                Some("a1"),
                Some(&["a1:INBOX:2".to_string()]),
                None,
                true,
                50,
                0,
            )
            .unwrap();
        let ids: Vec<&str> = restricted.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["a1:INBOX:2"]);
        cleanup(&path);
    }

    #[test]
    fn evaluate_filter_restricted_to_empty_ids_returns_empty_not_all() {
        let (db, path) = seeded_db();
        seed_message(&db, "a1:INBOX:1", "invoice due", "a@x.test", 1, false);
        let restricted = db
            .evaluate_filter_restricted(&group_invoices(), Some("a1"), Some(&[]), None, true, 50, 0)
            .unwrap();
        assert!(restricted.is_empty());
        cleanup(&path);
    }

    /// Regression for the review-2 finding: a top-level `BoolOp::Or` group
    /// must not escape the account/message-id scope predicates it is ANDed
    /// with. Before the fix, `evaluate_filter_restricted` built
    /// `WHERE a OR b AND m.account_id = ? AND m.id IN (...)`, which SQLite
    /// parses as `a OR (b AND ... AND ...)` — the left arm of the OR is
    /// unscoped, so a message in a *different* account, outside the
    /// message-id batch, matched anyway. Reproduces the exact shape the
    /// reviewer found: OR(IsStarred=true, HasAttachments=true).
    #[test]
    fn top_level_or_group_does_not_leak_account_or_batch_scope() {
        let (db, path) = seeded_db();
        db.upsert_account(&Account {
            id: "a2".into(),
            email: "them@x.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.upsert_mailbox(&Mailbox {
            id: "a2:INBOX".into(),
            account_id: "a2".into(),
            path: "INBOX".into(),
            name: "INBOX".into(),
            role: Some("inbox".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();

        // m1: account a1, unstarred, no attachments — matches neither OR arm.
        seed_message_full(
            &db,
            "a1",
            "a1:INBOX",
            "a1:INBOX:1",
            "hello",
            "a@x.test",
            1,
            false,
            false,
        );
        // m2: account a2, starred — matches the OR, but is in a DIFFERENT
        // account and outside any message-id batch scoped to m1.
        seed_message_full(
            &db,
            "a2",
            "a2:INBOX",
            "a2:INBOX:2",
            "hello",
            "b@x.test",
            2,
            true,
            false,
        );

        let or_group = FilterGroup {
            op: BoolOp::Or,
            conditions: vec![
                FilterCondition {
                    field: FilterField::IsStarred,
                    operator: FilterOperator::Is,
                    value: FilterValue::Bool(true),
                },
                FilterCondition {
                    field: FilterField::HasAttachments,
                    operator: FilterOperator::Is,
                    value: FilterValue::Bool(true),
                },
            ],
            groups: vec![],
        };

        // Account-scoped to a1: must not see a2's starred message.
        let account_scoped = db
            .evaluate_filter(&or_group, Some("a1"), None, true, 50, 0)
            .unwrap();
        assert!(
            account_scoped.is_empty(),
            "account scope leaked: {:?}",
            account_scoped.iter().map(|m| &m.id).collect::<Vec<_>>()
        );

        // Batch-scoped to just m1 (no account filter): must not see a2's
        // starred message either.
        let batch_scoped = db
            .evaluate_filter_restricted(
                &or_group,
                None,
                Some(&["a1:INBOX:1".to_string()]),
                None,
                true,
                50,
                0,
            )
            .unwrap();
        assert!(
            batch_scoped.is_empty(),
            "batch scope leaked: {:?}",
            batch_scoped.iter().map(|m| &m.id).collect::<Vec<_>>()
        );

        cleanup(&path);
    }

    // ---- MM-1: none groups, single-column FTS, relative dates, limits ----

    fn leaf(field: FilterField, operator: FilterOperator, value: FilterValue) -> FilterCondition {
        FilterCondition {
            field,
            operator,
            value,
        }
    }

    fn group(
        op: BoolOp,
        conditions: Vec<FilterCondition>,
        groups: Vec<FilterGroup>,
    ) -> FilterGroup {
        FilterGroup {
            op,
            conditions,
            groups,
        }
    }

    fn subjects(db: &Database, g: &FilterGroup) -> Vec<String> {
        let mut v: Vec<String> = db
            .evaluate_filter(g, None, None, false, 100, 0)
            .unwrap()
            .into_iter()
            .map(|h| h.subject.unwrap_or_default())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn none_group_excludes_every_member_and_keeps_null_columns() {
        let (db, path) = seeded_db();
        seed_message(&db, "a1:INBOX:1", "Invoice A", "billing@x.test", 100, false);
        seed_message(&db, "a1:INBOX:2", "Hello", "alice@x.test", 100, false);
        seed_message(&db, "a1:INBOX:3", "Lunch", "bob@x.test", 100, false);
        // size_bytes is NULL on all three: a naive NOT(size > 5) would drop
        // them all (NOT NULL is NULL); the IFNULL guard keeps them.
        let none_big = group(
            BoolOp::None,
            vec![leaf(
                FilterField::SizeBytes,
                FilterOperator::Gt,
                FilterValue::Number(5),
            )],
            vec![],
        );
        assert_eq!(subjects(&db, &none_big).len(), 3);
        let none_of = group(
            BoolOp::None,
            vec![
                leaf(
                    FilterField::From,
                    FilterOperator::Contains,
                    FilterValue::Text("billing".into()),
                ),
                leaf(
                    FilterField::Subject,
                    FilterOperator::Contains,
                    FilterValue::Text("lunch".into()),
                ),
            ],
            vec![],
        );
        assert_eq!(subjects(&db, &none_of), vec!["Hello".to_string()]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn none_inside_any_inside_all_nests_three_deep() {
        let (db, path) = seeded_db();
        seed_message(&db, "a1:INBOX:1", "Invoice A", "billing@x.test", 100, false);
        seed_message(&db, "a1:INBOX:2", "Hello", "alice@x.test", 100, true);
        seed_message(&db, "a1:INBOX:3", "Lunch", "bob@x.test", 100, false);
        // all( unread, any( from bob, none( from billing ) ) )
        let g = group(
            BoolOp::And,
            vec![leaf(
                FilterField::IsRead,
                FilterOperator::Is,
                FilterValue::Bool(false),
            )],
            vec![group(
                BoolOp::Or,
                vec![leaf(
                    FilterField::From,
                    FilterOperator::Contains,
                    FilterValue::Text("bob".into()),
                )],
                vec![group(
                    BoolOp::None,
                    vec![leaf(
                        FilterField::From,
                        FilterOperator::Contains,
                        FilterValue::Text("billing".into()),
                    )],
                    vec![],
                )],
            )],
        );
        // unread & (bob | not-billing): Lunch (bob). Invoice is billing; Hello is read.
        assert_eq!(subjects(&db, &g), vec!["Lunch".to_string()]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn subject_condition_does_not_match_the_body() {
        let (db, path) = seeded_db();
        db.upsert_message(&NewMessage {
            id: "a1:INBOX:1".into(),
            account_id: "a1".into(),
            mailbox_id: "a1:INBOX".into(),
            provider_uid: Some(1),
            rfc822_message_id: Some("<1@x.test>".into()),
            subject: Some("Plain".into()),
            from_name: None,
            from_address: Some("a@x.test".into()),
            sent_at: Some(100),
            snippet: None,
            size_bytes: None,
            has_attachments: false,
            attachments: None,
            is_read: false,
            is_starred: false,
            addresses: vec![],
            text_body: Some("the invoice is attached".into()),
            html_body: None,
            sanitized_html: None,
            has_remote_content: false,
            references: vec![],
        })
        .unwrap();
        let in_subject = group(
            BoolOp::And,
            vec![leaf(
                FilterField::Subject,
                FilterOperator::Contains,
                FilterValue::Text("invoice".into()),
            )],
            vec![],
        );
        let in_body = group(
            BoolOp::And,
            vec![leaf(
                FilterField::Body,
                FilterOperator::Contains,
                FilterValue::Text("invoice".into()),
            )],
            vec![],
        );
        assert!(
            subjects(&db, &in_subject).is_empty(),
            "body text must not satisfy a subject condition"
        );
        assert_eq!(subjects(&db, &in_body), vec!["Plain".to_string()]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn subject_equals_and_starts_with_use_the_stored_subject() {
        let (db, path) = seeded_db();
        seed_message(&db, "a1:INBOX:1", "Invoice 42", "a@x.test", 100, false);
        seed_message(&db, "a1:INBOX:2", "Re: Invoice 42", "a@x.test", 100, false);
        let starts = group(
            BoolOp::And,
            vec![leaf(
                FilterField::Subject,
                FilterOperator::StartsWith,
                FilterValue::Text("Invoice".into()),
            )],
            vec![],
        );
        assert_eq!(subjects(&db, &starts), vec!["Invoice 42".to_string()]);
        let equals = group(
            BoolOp::And,
            vec![leaf(
                FilterField::Subject,
                FilterOperator::Equals,
                FilterValue::Text("Re: Invoice 42".into()),
            )],
            vec![],
        );
        assert_eq!(subjects(&db, &equals), vec!["Re: Invoice 42".to_string()]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn relative_dates_track_now() {
        let (db, path) = seeded_db();
        let now = now_epoch();
        seed_message(
            &db,
            "a1:INBOX:1",
            "Recent",
            "a@x.test",
            now - 2 * 86_400,
            false,
        );
        seed_message(
            &db,
            "a1:INBOX:2",
            "Old",
            "a@x.test",
            now - 40 * 86_400,
            false,
        );
        let within = group(
            BoolOp::And,
            vec![leaf(
                FilterField::Date,
                FilterOperator::WithinLastDays,
                FilterValue::Number(7),
            )],
            vec![],
        );
        let older = group(
            BoolOp::And,
            vec![leaf(
                FilterField::Date,
                FilterOperator::OlderThanDays,
                FilterValue::Number(30),
            )],
            vec![],
        );
        assert_eq!(subjects(&db, &within), vec!["Recent".to_string()]);
        assert_eq!(subjects(&db, &older), vec!["Old".to_string()]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn account_scope_condition_and_hostile_text_are_safe() {
        let (db, path) = seeded_db();
        seed_message(&db, "a1:INBOX:1", "Hello", "a@x.test", 100, false);
        let other = group(
            BoolOp::And,
            vec![leaf(
                FilterField::AccountId,
                FilterOperator::In,
                FilterValue::List(vec!["nope".into()]),
            )],
            vec![],
        );
        assert!(subjects(&db, &other).is_empty());
        let mine = group(
            BoolOp::And,
            vec![leaf(
                FilterField::AccountId,
                FilterOperator::Equals,
                FilterValue::Text("a1".into()),
            )],
            vec![],
        );
        assert_eq!(subjects(&db, &mine), vec!["Hello".to_string()]);
        for hostile in ["'\";DROP TABLE messages;--", "NEAR(", "50%_off", "a\\b"] {
            for field in [
                FilterField::Subject,
                FilterField::Body,
                FilterField::From,
                FilterField::AttachmentName,
            ] {
                let g = group(
                    BoolOp::Or,
                    vec![leaf(
                        field,
                        FilterOperator::Contains,
                        FilterValue::Text(hostile.into()),
                    )],
                    vec![],
                );
                let _ = subjects(&db, &g);
            }
        }
        assert_eq!(
            subjects(&db, &mine),
            vec!["Hello".to_string()],
            "tables intact"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn depth_and_size_limits_are_clear_errors() {
        let (db, path) = seeded_db();
        let mut g = group(
            BoolOp::And,
            vec![leaf(
                FilterField::IsRead,
                FilterOperator::Is,
                FilterValue::Bool(false),
            )],
            vec![],
        );
        for _ in 0..crate::filter_sql::MAX_GROUP_DEPTH {
            g = group(BoolOp::Or, vec![], vec![g]);
        }
        let err = db
            .evaluate_filter(&g, None, None, false, 10, 0)
            .unwrap_err();
        assert!(err.0.contains("nested"), "{}", err.0);
        let wide = group(
            BoolOp::Or,
            (0..=crate::filter_sql::MAX_CONDITIONS)
                .map(|_| {
                    leaf(
                        FilterField::IsRead,
                        FilterOperator::Is,
                        FilterValue::Bool(true),
                    )
                })
                .collect(),
            vec![],
        );
        assert!(db.evaluate_filter(&wide, None, None, false, 10, 0).is_err());
        let _ = std::fs::remove_file(path);
    }
}
