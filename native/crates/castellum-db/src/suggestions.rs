// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//! Suggestions you accept (migration 0046, AW2) - storage and the atomic
//! accept/dismiss transitions.
//!
//! Invariants pinned by the tests below:
//! - A proposal is a `suggestions` row, never a `work_items` row: it is
//!   invisible to work-item filters, link lookups and counts until accepted.
//! - Evidence is IDS ONLY (message, thread, extraction row, RFC822 id). No
//!   body, snippet or subject text is stored; the payload holds only the
//!   proposed title / due date / target state.
//! - `dedupe_key` is UNIQUE across every status, so a producer that runs
//!   twice yields one row, and a dismissed key never produces a new proposal.
//! - Accept runs in ONE transaction: the work item (or state change), its
//!   evidence links and the suggestion's own status either all land or none
//!   do. Title and notes of a created item are user-owned from that moment -
//!   nothing here ever rewrites them afterwards.
//! - Accept and dismiss are idempotent; accepting a dismissed suggestion (or
//!   dismissing an accepted one) is an error.
//!
//! The producers that decide WHAT to propose (heuristics, clock, the
//! self-addressed exclusion) live in `castellum-core::suggestions`; this
//! layer stores whatever it is handed and calls no model.

use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::links::EntityKind;
use crate::project_timeline::{insert_entry, short, TimelineKind};
use crate::work_items::{row_to_item, validate_state, ITEM_COLUMNS};
use crate::{Database, DbError, WorkItem, WORK_ITEM_LINK_RELATION};

pub const SUGGESTION_KINDS: [&str; 3] = ["work_item", "waiting_for_reply", "state_change"];
pub const SUGGESTION_STATUSES: [&str; 3] = ["proposed", "accepted", "dismissed"];
pub const SUGGESTION_ORIGINS: [&str; 2] = ["heuristic", "ai_extraction"];

/// A stored suggestion. Serialized snake_case for the FFI/Swift boundary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Suggestion {
    pub id: String,
    pub kind: String,
    pub status: String,
    /// The proposal: `{title?, due_at?, to_state?}`.
    pub payload: serde_json::Value,
    // Evidence: ids only.
    pub message_id: Option<String>,
    pub thread_id: Option<String>,
    pub extraction_id: Option<i64>,
    pub rfc822_message_id: Option<String>,
    /// state_change: the work item to move.
    pub target_work_item_id: Option<String>,
    /// Set on accept: the work item this suggestion became.
    pub created_work_item_id: Option<String>,
    pub origin: String,
    pub confidence: Option<f64>,
    pub dedupe_key: String,
    pub created_at: i64,
    pub decided_at: Option<i64>,
}

/// What a producer hands to [`Database::propose_suggestion_at`].
#[derive(Clone, Copy, Debug, Default)]
pub struct NewSuggestion<'a> {
    pub kind: &'a str,
    pub payload_json: &'a str,
    pub message_id: Option<&'a str>,
    pub thread_id: Option<&'a str>,
    pub extraction_id: Option<i64>,
    pub rfc822_message_id: Option<&'a str>,
    pub target_work_item_id: Option<&'a str>,
    pub origin: &'a str,
    pub confidence: Option<f64>,
    pub dedupe_key: &'a str,
}

/// Listing filter; unknown keys are a parse error. All keys combine with AND.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuggestionFilter {
    pub status: Option<String>,
    pub kind: Option<String>,
    pub thread_id: Option<String>,
    /// Suggestions whose thread or message is linked to this project (either
    /// link direction), or whose target work item belongs to it.
    pub project_id: Option<String>,
    pub limit: Option<i64>,
}

/// Result of an accept: the (now accepted) suggestion and the work item it
/// created or changed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SuggestionAccepted {
    pub suggestion: Suggestion,
    pub work_item: Option<WorkItem>,
}

/// What a producer needs to know about a stored message.
#[derive(Clone, Debug, PartialEq)]
pub struct MessageContext {
    pub account_id: String,
    pub thread_id: Option<String>,
    pub from_address: Option<String>,
    pub rfc822_message_id: Option<String>,
}

const SUGGESTION_COLUMNS: &str = "id, kind, status, payload_json, message_id, thread_id, \
     extraction_id, rfc822_message_id, target_work_item_id, created_work_item_id, origin, \
     confidence, dedupe_key, created_at, decided_at";

fn row_to_suggestion(r: &rusqlite::Row<'_>) -> rusqlite::Result<Suggestion> {
    let payload: String = r.get(3)?;
    Ok(Suggestion {
        id: r.get(0)?,
        kind: r.get(1)?,
        status: r.get(2)?,
        payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
        message_id: r.get(4)?,
        thread_id: r.get(5)?,
        extraction_id: r.get(6)?,
        rfc822_message_id: r.get(7)?,
        target_work_item_id: r.get(8)?,
        created_work_item_id: r.get(9)?,
        origin: r.get(10)?,
        confidence: r.get(11)?,
        dedupe_key: r.get(12)?,
        created_at: r.get(13)?,
        decided_at: r.get(14)?,
    })
}

fn load(conn: &rusqlite::Connection, id: &str) -> Result<Option<Suggestion>, DbError> {
    Ok(conn
        .query_row(
            &format!("SELECT {SUGGESTION_COLUMNS} FROM suggestions WHERE id = ?1"),
            params![id],
            row_to_suggestion,
        )
        .optional()?)
}

fn load_item(conn: &rusqlite::Connection, id: &str) -> Result<Option<WorkItem>, DbError> {
    Ok(conn
        .query_row(
            &format!("SELECT {ITEM_COLUMNS} FROM work_items WHERE id = ?1"),
            params![id],
            row_to_item,
        )
        .optional()?)
}

/// Insert one manual `about` link (work item -> target), tagged with the
/// suggestion it came from as `suggestion:<id>` provenance (an id reference,
/// per the links-model grammar). Runs on the caller's transaction.
fn link_in_tx(
    conn: &rusqlite::Connection,
    work_item_id: &str,
    dst_type: &str,
    dst_id: &str,
    suggestion_id: &str,
    now: i64,
) -> Result<(), DbError> {
    conn.execute(
        "INSERT INTO links (src_type, src_id, dst_type, dst_id, relation, origin,
                            provenance, confidence, note, created_at, updated_at)
         VALUES ('work_item', ?1, ?2, ?3, ?4, 'manual', ?5, NULL, NULL, ?6, ?6)
         ON CONFLICT(src_type, src_id, dst_type, dst_id, relation) DO NOTHING",
        params![
            work_item_id,
            dst_type,
            dst_id,
            WORK_ITEM_LINK_RELATION,
            format!("suggestion:{suggestion_id}"),
            now
        ],
    )?;
    Ok(())
}

impl Database {
    /// Store a proposal. Idempotent on `dedupe_key` across EVERY status:
    /// returns `Some(new row)` when inserted, `None` when the key already
    /// exists (proposed, accepted or dismissed - a dismissed key never
    /// produces a new proposal).
    pub fn propose_suggestion_at(
        &self,
        s: &NewSuggestion<'_>,
        now: i64,
    ) -> Result<Option<Suggestion>, DbError> {
        if !SUGGESTION_KINDS.contains(&s.kind) {
            return Err(DbError(format!("invalid suggestion kind {:?}", s.kind)));
        }
        if !SUGGESTION_ORIGINS.contains(&s.origin) {
            return Err(DbError(format!("invalid suggestion origin {:?}", s.origin)));
        }
        if s.dedupe_key.is_empty() {
            return Err(DbError("suggestion dedupe_key must not be empty".into()));
        }
        serde_json::from_str::<serde_json::Value>(s.payload_json)
            .map_err(|e| DbError(format!("suggestion payload is not JSON: {e}")))?;
        let conn = self.lock_conn();
        let id = Uuid::new_v4().to_string();
        let n = conn.execute(
            "INSERT INTO suggestions
                 (id, kind, status, payload_json, message_id, thread_id, extraction_id,
                  rfc822_message_id, target_work_item_id, origin, confidence,
                  dedupe_key, created_at)
             VALUES (?1, ?2, 'proposed', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(dedupe_key) DO NOTHING",
            params![
                id,
                s.kind,
                s.payload_json,
                s.message_id,
                s.thread_id,
                s.extraction_id,
                s.rfc822_message_id,
                s.target_work_item_id,
                s.origin,
                s.confidence,
                s.dedupe_key,
                now
            ],
        )?;
        if n == 0 {
            return Ok(None);
        }
        load(&conn, &id)
    }

    /// One suggestion by id.
    pub fn get_suggestion(&self, id: &str) -> Result<Option<Suggestion>, DbError> {
        load(&self.lock_conn(), id)
    }

    /// Suggestions matching the filter, newest first (created_at, then
    /// insertion order).
    pub fn list_suggestions(&self, f: &SuggestionFilter) -> Result<Vec<Suggestion>, DbError> {
        if let Some(st) = &f.status {
            if !SUGGESTION_STATUSES.contains(&st.as_str()) {
                return Err(DbError(format!("invalid suggestion status {st:?}")));
            }
        }
        if let Some(k) = &f.kind {
            if !SUGGESTION_KINDS.contains(&k.as_str()) {
                return Err(DbError(format!("invalid suggestion kind {k:?}")));
            }
        }
        let mut wheres: Vec<String> = Vec::new();
        let mut args: Vec<Value> = Vec::new();
        if let Some(st) = &f.status {
            args.push(Value::Text(st.clone()));
            wheres.push(format!("s.status = ?{}", args.len()));
        }
        if let Some(k) = &f.kind {
            args.push(Value::Text(k.clone()));
            wheres.push(format!("s.kind = ?{}", args.len()));
        }
        if let Some(t) = &f.thread_id {
            args.push(Value::Text(t.clone()));
            wheres.push(format!("s.thread_id = ?{}", args.len()));
        }
        if let Some(p) = &f.project_id {
            args.push(Value::Text(p.clone()));
            let n = args.len();
            wheres.push(format!(
                "(EXISTS (SELECT 1 FROM links l
                          WHERE ((l.src_type = 'project' AND l.src_id = ?{n}
                                  AND ((l.dst_type = 'thread' AND l.dst_id = s.thread_id)
                                    OR (l.dst_type = 'message' AND l.dst_id = s.message_id)))
                              OR (l.dst_type = 'project' AND l.dst_id = ?{n}
                                  AND ((l.src_type = 'thread' AND l.src_id = s.thread_id)
                                    OR (l.src_type = 'message' AND l.src_id = s.message_id)))))
                  OR EXISTS (SELECT 1 FROM work_items w
                             WHERE w.id = s.target_work_item_id AND w.project_id = ?{n}))"
            ));
        }
        let where_sql = if wheres.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", wheres.join(" AND "))
        };
        let limit_sql = match f.limit {
            Some(n) if n > 0 => format!("LIMIT {n}"),
            _ => String::new(),
        };
        let cols = SUGGESTION_COLUMNS
            .split(", ")
            .map(|c| format!("s.{}", c.trim()))
            .collect::<Vec<_>>()
            .join(", ");
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {cols} FROM suggestions s {where_sql}
             ORDER BY s.created_at DESC, s.rowid DESC {limit_sql}"
        ))?;
        let rows = stmt
            .query_map(params_from_iter(args.iter()), row_to_suggestion)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Number of suggestions still waiting for a decision (the future badge).
    pub fn count_proposed_suggestions(&self) -> Result<i64, DbError> {
        Ok(self.lock_conn().query_row(
            "SELECT COUNT(*) FROM suggestions WHERE status = 'proposed'",
            [],
            |r| r.get(0),
        )?)
    }

    /// Mark a suggestion dismissed. Idempotent (a second call keeps the first
    /// `decided_at`); dismissing an accepted suggestion is an error. The
    /// dedupe key stays, so the same proposal is never made again.
    pub fn dismiss_suggestion_at(&self, id: &str, now: i64) -> Result<Suggestion, DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let Some(s) = load(&tx, id)? else {
            return Err(DbError(format!("suggestion {id} not found")));
        };
        match s.status.as_str() {
            "dismissed" => return Ok(s),
            "accepted" => {
                return Err(DbError(format!(
                    "suggestion {id} was already accepted and cannot be dismissed"
                )))
            }
            _ => {}
        }
        tx.execute(
            "UPDATE suggestions SET status = 'dismissed', decided_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        let out = load(&tx, id)?.ok_or_else(|| DbError("suggestion vanished".into()))?;
        tx.commit()?;
        Ok(out)
    }

    /// Accept a suggestion, atomically.
    ///
    /// - `work_item` / `waiting_for_reply`: create the work item (state
    ///   `needs_reply` / `waiting` with `waiting_since = now`, no project,
    ///   empty notes, the proposed title and due date), link it to the
    ///   evidence thread and message (manual `about` links whose provenance
    ///   is `suggestion:<id>`), and record the item on the suggestion.
    /// - `state_change`: move the target item out of `waiting` (clearing
    ///   `waiting_since`); an item that is no longer waiting is an error.
    ///
    /// Idempotent: accepting an accepted suggestion returns it unchanged
    /// (no second work item). Accepting a dismissed one is an error. Any
    /// failure rolls the whole transaction back.
    pub fn accept_suggestion_at(&self, id: &str, now: i64) -> Result<SuggestionAccepted, DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let Some(s) = load(&tx, id)? else {
            return Err(DbError(format!("suggestion {id} not found")));
        };
        match s.status.as_str() {
            "accepted" => {
                let item_id = s
                    .created_work_item_id
                    .as_deref()
                    .or(s.target_work_item_id.as_deref());
                let work_item = match item_id {
                    Some(w) => load_item(&tx, w)?,
                    None => None,
                };
                return Ok(SuggestionAccepted {
                    suggestion: s,
                    work_item,
                });
            }
            "dismissed" => {
                return Err(DbError(format!(
                    "suggestion {id} was dismissed and cannot be accepted"
                )))
            }
            _ => {}
        }
        let work_item_id = match s.kind.as_str() {
            "work_item" | "waiting_for_reply" => {
                let title = s
                    .payload
                    .get("title")
                    .and_then(|t| t.as_str())
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .ok_or_else(|| DbError("suggestion payload has no title".into()))?
                    .to_string();
                let due_at = s.payload.get("due_at").and_then(|d| d.as_i64());
                let (state, waiting_since) = if s.kind == "waiting_for_reply" {
                    ("waiting", Some(now))
                } else {
                    ("needs_reply", None)
                };
                let new_id = Uuid::new_v4().to_string();
                tx.execute(
                    "INSERT INTO work_items
                         (id, project_id, title, notes, state, position, created_at,
                          updated_at, due_at, waiting_since, completed_at,
                          delegated_to_contact_id)
                     VALUES (?1, NULL, ?2, '', ?3, 0, ?4, ?4, ?5, ?6, NULL, NULL)",
                    params![new_id, title, state, now, due_at, waiting_since],
                )?;
                // Evidence, resolved late for a just-sent message whose Sent
                // copy has synced since the proposal was made.
                let (mut message_id, mut thread_id) = (s.message_id.clone(), s.thread_id.clone());
                if let Some(rfc) = s.rfc822_message_id.as_deref() {
                    if message_id.is_none() || thread_id.is_none() {
                        let account = s.payload.get("account_id").and_then(|a| a.as_str());
                        if let Some((m, t)) = resolve_rfc822(&tx, account, rfc)? {
                            message_id = message_id.or(Some(m));
                            thread_id = thread_id.or(t);
                        }
                    }
                }
                if let Some(t) = thread_id.as_deref() {
                    link_in_tx(&tx, &new_id, "thread", t, id, now)?;
                }
                if let Some(m) = message_id.as_deref() {
                    let exists = tx
                        .query_row("SELECT 1 FROM messages WHERE id = ?1", params![m], |_| {
                            Ok(())
                        })
                        .optional()?
                        .is_some();
                    // Evidence may have been purged since the proposal: the
                    // item still stands, it just loses that one link.
                    if exists {
                        link_in_tx(&tx, &new_id, "message", m, id, now)?;
                        crate::provenance::record_in(&tx, &new_id, "message", m, now)?;
                    }
                }
                crate::provenance::record_in(&tx, &new_id, "suggestion", id, now)?;
                tx.execute(
                    "UPDATE suggestions
                     SET status = 'accepted', decided_at = ?2, created_work_item_id = ?3,
                         message_id = ?4, thread_id = ?5
                     WHERE id = ?1",
                    params![id, now, new_id, message_id, thread_id],
                )?;
                new_id
            }
            "state_change" => {
                let target = s
                    .target_work_item_id
                    .clone()
                    .ok_or_else(|| DbError("state_change suggestion has no target".into()))?;
                let to_state = s
                    .payload
                    .get("to_state")
                    .and_then(|t| t.as_str())
                    .unwrap_or("needs_reply")
                    .to_string();
                validate_state(&to_state)?;
                if to_state == "delegated" {
                    return Err(DbError(
                        "a suggestion cannot move an item to delegated".into(),
                    ));
                }
                let item = load_item(&tx, &target)?
                    .ok_or_else(|| DbError(format!("work item {target} not found")))?;
                if item.state != "waiting" {
                    return Err(DbError(format!(
                        "work item {target} is no longer waiting (now {})",
                        item.state
                    )));
                }
                let completed_at = (to_state == "done").then_some(now);
                tx.execute(
                    "UPDATE work_items
                     SET state = ?2, waiting_since = NULL, completed_at = ?3, updated_at = ?4
                     WHERE id = ?1",
                    params![target, to_state, completed_at, now],
                )?;
                if let Some(pid) = item.project_id.as_deref() {
                    insert_entry(
                        &tx,
                        pid,
                        TimelineKind::WorkItemChange,
                        &format!(
                            "Work item {}: {} -> {}",
                            short(&item.title),
                            item.state,
                            to_state
                        ),
                        Some((EntityKind::WorkItem, &target)),
                        now,
                    )?;
                }
                tx.execute(
                    "UPDATE suggestions SET status = 'accepted', decided_at = ?2 WHERE id = ?1",
                    params![id, now],
                )?;
                target
            }
            other => return Err(DbError(format!("unknown suggestion kind {other:?}"))),
        };
        let suggestion = load(&tx, id)?.ok_or_else(|| DbError("suggestion vanished".into()))?;
        let work_item = load_item(&tx, &work_item_id)?;
        tx.commit()?;
        Ok(SuggestionAccepted {
            suggestion,
            work_item,
        })
    }

    /// Fill in the message/thread evidence of suggestions made when only the
    /// sent message's RFC822 id was known (the Sent copy had not synced yet).
    ///
    /// - `proposed` rows just gain the evidence ids.
    /// - `accepted` rows (the user accepted before the Sent copy synced, so
    ///   the work item was created with no thread link) gain the evidence AND
    ///   the missing `about` links on the created work item, so the
    ///   reply-arrived producer can find the thread later.
    ///
    /// Idempotent; returns rows updated. `now` stamps the new links.
    pub fn attach_sent_evidence(
        &self,
        rfc822_message_id: &str,
        message_id: &str,
        thread_id: Option<&str>,
        now: i64,
    ) -> Result<usize, DbError> {
        // Message-IDs AND the From header are sender-controlled (any reply
        // quotes the former; anyone can forge the latter), so the evidence
        // must be Marc's own copy by a property the sender cannot set: it
        // sits in a Sent-role mailbox of the SAME account the suggestion was
        // made for (`account_id` in its payload; a NULL/legacy payload never
        // matches). All checks run inside the transaction.
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let account: Option<String> = tx
            .query_row(
                "SELECT m.account_id FROM messages m JOIN mailboxes b ON b.id = m.mailbox_id
                 WHERE m.id = ?1 AND b.role = 'sent'",
                params![message_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(account) = account else {
            return Ok(0);
        };
        let mut n = tx.execute(
            "UPDATE suggestions
             SET message_id = COALESCE(message_id, ?2), thread_id = COALESCE(thread_id, ?3)
             WHERE rfc822_message_id = ?1 AND status = 'proposed'
               AND json_extract(payload_json, '$.account_id') = ?4
               AND (message_id IS NULL OR thread_id IS NULL)",
            params![rfc822_message_id, message_id, thread_id, account],
        )?;
        let accepted: Vec<(String, String)> = {
            let mut stmt = tx.prepare(
                "SELECT id, created_work_item_id FROM suggestions
                 WHERE rfc822_message_id = ?1 AND status = 'accepted'
                   AND created_work_item_id IS NOT NULL
                   AND json_extract(payload_json, '$.account_id') = ?2
                   AND (message_id IS NULL OR thread_id IS NULL)",
            )?;
            let rows = stmt.query_map(params![rfc822_message_id, account], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
            rows.collect::<Result<_, _>>()?
        };
        for (sid, item_id) in accepted {
            tx.execute(
                "UPDATE suggestions
                 SET message_id = COALESCE(message_id, ?2), thread_id = COALESCE(thread_id, ?3)
                 WHERE id = ?1",
                params![sid, message_id, thread_id],
            )?;
            // The work item may have been deleted since: only link one that
            // still exists, and count only rows actually linked.
            let exists = tx
                .query_row(
                    "SELECT 1 FROM work_items WHERE id = ?1",
                    params![item_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if exists {
                if let Some(t) = thread_id {
                    link_in_tx(&tx, &item_id, "thread", t, &sid, now)?;
                }
                link_in_tx(&tx, &item_id, "message", message_id, &sid, now)?;
                crate::provenance::record_in(&tx, &item_id, "message", message_id, now)?;
                n += 1;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// Producer helper: a message's account, thread, sender and RFC822 id.
    pub fn message_context(&self, message_id: &str) -> Result<Option<MessageContext>, DbError> {
        Ok(self
            .lock_conn()
            .query_row(
                "SELECT account_id, thread_id, from_address, rfc822_message_id
                 FROM messages WHERE id = ?1",
                params![message_id],
                |r| {
                    Ok(MessageContext {
                        account_id: r.get(0)?,
                        thread_id: r.get(1)?,
                        from_address: r.get(2)?,
                        rfc822_message_id: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// Producer helper: the stored message (and its thread) carrying this
    /// RFC822 Message-ID in an account, if any.
    pub fn message_by_rfc822(
        &self,
        account_id: &str,
        rfc822_message_id: &str,
    ) -> Result<Option<(String, Option<String>)>, DbError> {
        Ok(self
            .lock_conn()
            .query_row(
                "SELECT id, thread_id FROM messages
                 WHERE account_id = ?1 AND rfc822_message_id = ?2 ORDER BY id LIMIT 1",
                params![account_id, rfc822_message_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// Producer helper: every address that is "me" for an account - the
    /// account address plus its identities - lowercased.
    pub fn own_addresses(&self, account_id: &str) -> Result<Vec<String>, DbError> {
        let conn = self.lock_conn();
        let mut out: Vec<String> = Vec::new();
        let mut stmt = conn.prepare(
            "SELECT email FROM accounts WHERE id = ?1
             UNION SELECT email FROM account_identities WHERE account_id = ?1",
        )?;
        for row in stmt.query_map(params![account_id], |r| r.get::<_, String>(0))? {
            out.push(row?.to_ascii_lowercase());
        }
        Ok(out)
    }
}

/// Resolve an RFC822 Message-ID to a stored message id and its thread, but
/// only to a message of `account` that sits in its Sent-role mailbox (the
/// Sent copy; the From header is forgeable so it is not trusted). A suggestion without an account (legacy rows)
/// resolves to nothing: Message-IDs are sender-controlled.
fn resolve_rfc822(
    conn: &rusqlite::Connection,
    account: Option<&str>,
    rfc822: &str,
) -> Result<Option<(String, Option<String>)>, DbError> {
    let Some(account) = account else {
        return Ok(None);
    };
    Ok(conn
        .query_row(
            "SELECT id, thread_id FROM messages
             WHERE account_id = ?1 AND rfc822_message_id = ?2
               AND mailbox_id IN (SELECT id FROM mailboxes WHERE account_id = ?1 AND role = 'sent')
             ORDER BY id LIMIT 1",
            params![account, rfc822],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EntityKind, WorkItemFilter};
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A fresh temp-dir database seeded with account `acc`, message `m1` and
    /// a project `p1` (never the real profile).
    fn fresh_db() -> Database {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-suggestions-test-{}-{}.db",
            std::process::id(),
            n
        ));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
        let db = Database::open(&path).expect("open");
        db.lock_conn()
            .execute_batch(
                "INSERT INTO accounts (id, email, display_name, provider)
                     VALUES ('acc', 'me@local.test', 'Me', 'imap');
                 INSERT INTO mailboxes (id, account_id, path, name, role)
                     VALUES ('acc:INBOX', 'acc', 'INBOX', 'INBOX', 'inbox');
                 INSERT INTO messages (id, account_id, mailbox_id, provider_uid,
                                       rfc822_message_id, thread_id, from_address)
                     VALUES ('m1', 'acc', 'acc:INBOX', 1, '<1@x.test>', 't1', 'a@x.test');
                 INSERT INTO mailboxes (id, account_id, path, name, role)
                     VALUES ('acc:Sent', 'acc', 'Sent', 'Sent', 'sent');
                 INSERT INTO messages (id, account_id, mailbox_id, provider_uid,
                                       rfc822_message_id, thread_id, from_address)
                     VALUES ('seed_own', 'acc', 'acc:Sent', 2, '<own@x.test>', 't1', 'Me@Local.test');
                 INSERT INTO projects (id, name, status, position, created_at, updated_at,
                                       last_activity_at)
                     VALUES ('p1', 'P', 'active', 0, 0, 0, 0);",
            )
            .expect("seed");
        db
    }

    fn new<'a>(kind: &'a str, key: &'a str) -> NewSuggestion<'a> {
        NewSuggestion {
            kind,
            payload_json: r#"{"title":"Call Jane","due_at":1800000000}"#,
            message_id: Some("m1"),
            thread_id: Some("t1"),
            extraction_id: Some(5),
            origin: "ai_extraction",
            dedupe_key: key,
            ..Default::default()
        }
    }

    #[test]
    fn schema_holds_ids_only_evidence() {
        let db = fresh_db();
        let conn = db.lock_conn();
        let cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('suggestions') ORDER BY cid")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for banned in ["body", "text", "snippet", "subject"] {
            assert!(
                !cols.iter().any(|c| c.contains(banned)),
                "suggestions must not copy mail content: {cols:?}"
            );
        }
        for evidence in ["message_id", "thread_id", "extraction_id"] {
            assert!(cols.iter().any(|c| c == evidence), "{evidence} missing");
        }
    }

    #[test]
    fn propose_is_idempotent_on_the_dedupe_key() {
        let db = fresh_db();
        assert!(db
            .propose_suggestion_at(&new("work_item", "k1"), 10)
            .unwrap()
            .is_some());
        assert!(db
            .propose_suggestion_at(&new("work_item", "k1"), 11)
            .unwrap()
            .is_none());
        assert_eq!(db.list_suggestions(&Default::default()).unwrap().len(), 1);
        assert!(db
            .propose_suggestion_at(&new("work_item", "bad"), 10)
            .is_ok());
        let bad = NewSuggestion {
            kind: "nope",
            ..new("work_item", "k9")
        };
        assert!(db.propose_suggestion_at(&bad, 1).is_err());
    }

    #[test]
    fn accept_creates_item_and_both_links_and_is_idempotent() {
        let db = fresh_db();
        let s = db
            .propose_suggestion_at(&new("work_item", "k1"), 10)
            .unwrap()
            .unwrap();
        // A proposal is not a work item.
        assert!(db
            .list_work_items_filtered(&WorkItemFilter::default())
            .unwrap()
            .is_empty());
        let a = db.accept_suggestion_at(&s.id, 20).unwrap();
        let w = a.work_item.expect("item");
        assert_eq!(
            (w.title.as_str(), w.state.as_str(), w.due_at, w.project_id),
            ("Call Jane", "needs_reply", Some(1_800_000_000), None)
        );
        assert_eq!(a.suggestion.status, "accepted");
        assert_eq!(a.suggestion.decided_at, Some(20));
        assert_eq!(a.suggestion.created_work_item_id.as_deref(), Some(&*w.id));
        assert_eq!(
            db.work_items_for(EntityKind::Thread, "t1").unwrap().len(),
            1
        );
        assert_eq!(
            db.work_items_for(EntityKind::Message, "m1").unwrap().len(),
            1
        );
        let link = db
            .link_between(
                crate::LinkEnd::new(EntityKind::WorkItem, &w.id),
                crate::LinkEnd::new(EntityKind::Thread, "t1"),
                WORK_ITEM_LINK_RELATION,
            )
            .unwrap()
            .expect("link");
        assert_eq!(
            link.provenance.as_deref(),
            Some(format!("suggestion:{}", s.id).as_str())
        );
        // Second accept: same item, no duplicate.
        let again = db.accept_suggestion_at(&s.id, 99).unwrap();
        assert_eq!(again.work_item.unwrap().id, w.id);
        assert_eq!(again.suggestion.decided_at, Some(20));
        assert_eq!(
            db.list_work_items_filtered(&WorkItemFilter::default())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn waiting_for_reply_accept_stamps_waiting_since() {
        let db = fresh_db();
        let s = db
            .propose_suggestion_at(&new("waiting_for_reply", "k1"), 10)
            .unwrap()
            .unwrap();
        let w = db
            .accept_suggestion_at(&s.id, 77)
            .unwrap()
            .work_item
            .unwrap();
        assert_eq!((w.state.as_str(), w.waiting_since), ("waiting", Some(77)));
    }

    #[test]
    fn accept_failure_mid_way_leaves_nothing_behind() {
        let db = fresh_db();
        let s = db
            .propose_suggestion_at(&new("work_item", "k1"), 10)
            .unwrap()
            .unwrap();
        // Force the link insert (after the work item insert) to fail.
        db.lock_conn()
            .execute_batch(
                "CREATE TRIGGER boom BEFORE INSERT ON links
                 BEGIN SELECT RAISE(ABORT, 'forced failure'); END;",
            )
            .unwrap();
        assert!(db.accept_suggestion_at(&s.id, 20).is_err());
        assert!(db
            .list_work_items_filtered(&WorkItemFilter::default())
            .unwrap()
            .is_empty());
        let after = db.get_suggestion(&s.id).unwrap().unwrap();
        assert_eq!(
            (
                after.status.as_str(),
                after.created_work_item_id,
                after.decided_at
            ),
            ("proposed", None, None)
        );
        // Once the fault clears, the accept works.
        db.lock_conn().execute_batch("DROP TRIGGER boom;").unwrap();
        assert!(db
            .accept_suggestion_at(&s.id, 21)
            .unwrap()
            .work_item
            .is_some());
    }

    #[test]
    fn dismiss_is_idempotent_blocks_accept_and_keeps_the_key() {
        let db = fresh_db();
        let s = db
            .propose_suggestion_at(&new("work_item", "k1"), 10)
            .unwrap()
            .unwrap();
        let d = db.dismiss_suggestion_at(&s.id, 30).unwrap();
        assert_eq!((d.status.as_str(), d.decided_at), ("dismissed", Some(30)));
        assert_eq!(
            db.dismiss_suggestion_at(&s.id, 40).unwrap().decided_at,
            Some(30)
        );
        assert!(db.accept_suggestion_at(&s.id, 50).is_err());
        // The key is spent: the producer cannot re-propose it.
        assert!(db
            .propose_suggestion_at(&new("work_item", "k1"), 60)
            .unwrap()
            .is_none());
        assert_eq!(db.count_proposed_suggestions().unwrap(), 0);
        assert!(db
            .list_work_items_filtered(&WorkItemFilter::default())
            .unwrap()
            .is_empty());
        // And an accepted one cannot be dismissed.
        let s2 = db
            .propose_suggestion_at(&new("work_item", "k2"), 10)
            .unwrap()
            .unwrap();
        db.accept_suggestion_at(&s2.id, 20).unwrap();
        assert!(db.dismiss_suggestion_at(&s2.id, 21).is_err());
        assert!(db.accept_suggestion_at("missing", 1).is_err());
    }

    #[test]
    fn state_change_moves_a_waiting_item_only_on_accept() {
        let db = fresh_db();
        db.lock_conn()
            .execute_batch(
                "INSERT INTO work_items (id, project_id, title, state, position, created_at,
                                         updated_at, waiting_since)
                 VALUES ('w1', 'p1', 'Chase', 'waiting', 0, 1, 1, 5);",
            )
            .unwrap();
        let s = db
            .propose_suggestion_at(
                &NewSuggestion {
                    kind: "state_change",
                    payload_json: r#"{"to_state":"needs_reply"}"#,
                    target_work_item_id: Some("w1"),
                    origin: "heuristic",
                    dedupe_key: "sc1",
                    thread_id: Some("t1"),
                    message_id: Some("m1"),
                    ..Default::default()
                },
                10,
            )
            .unwrap()
            .unwrap();
        let untouched = db.get_work_item("w1").unwrap().unwrap();
        assert_eq!(
            (
                untouched.state.as_str(),
                untouched.waiting_since,
                untouched.updated_at
            ),
            ("waiting", Some(5), 1)
        );
        let w = db
            .accept_suggestion_at(&s.id, 40)
            .unwrap()
            .work_item
            .unwrap();
        assert_eq!(
            (
                w.state.as_str(),
                w.waiting_since,
                w.updated_at,
                w.title.as_str()
            ),
            ("needs_reply", None, 40, "Chase")
        );
        // The suggestion does not claim to have CREATED the item.
        assert_eq!(
            db.get_suggestion(&s.id)
                .unwrap()
                .unwrap()
                .created_work_item_id,
            None
        );
    }

    #[test]
    fn accepted_state_change_writes_one_timeline_row_only_for_project_items() {
        let db = fresh_db();
        db.lock_conn()
            .execute_batch(
                "INSERT INTO work_items (id, project_id, title, state, position, created_at,
                                         updated_at, waiting_since)
                 VALUES ('w1', 'p1', 'Chase', 'waiting', 0, 1, 1, 5),
                        ('w2', NULL, 'Loose', 'waiting', 0, 1, 1, 5);",
            )
            .unwrap();
        for (target, key) in [("w1", "tl1"), ("w2", "tl2")] {
            let s = db
                .propose_suggestion_at(
                    &NewSuggestion {
                        kind: "state_change",
                        payload_json: r#"{"to_state":"needs_reply"}"#,
                        target_work_item_id: Some(target),
                        origin: "heuristic",
                        dedupe_key: key,
                        ..Default::default()
                    },
                    10,
                )
                .unwrap()
                .unwrap();
            db.accept_suggestion_at(&s.id, 40).unwrap();
        }
        let rows = db
            .list_timeline("p1", Some(&[TimelineKind::WorkItemChange]), 50, 0)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].summary.contains("waiting -> needs_reply"));
        let total: i64 = db
            .lock_conn()
            .query_row("SELECT COUNT(*) FROM project_timeline", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 1, "project-less item added a row");
    }

    #[test]
    fn state_change_for_an_item_that_stopped_waiting_errors() {
        let db = fresh_db();
        db.lock_conn()
            .execute_batch(
                "INSERT INTO work_items (id, project_id, title, state, position, created_at, updated_at)
                 VALUES ('w1', 'p1', 'X', 'done', 0, 1, 1);",
            )
            .unwrap();
        let s = db
            .propose_suggestion_at(
                &NewSuggestion {
                    kind: "state_change",
                    payload_json: "{}",
                    target_work_item_id: Some("w1"),
                    origin: "heuristic",
                    dedupe_key: "sc1",
                    ..Default::default()
                },
                10,
            )
            .unwrap()
            .unwrap();
        assert!(db.accept_suggestion_at(&s.id, 40).is_err());
        assert_eq!(
            db.get_suggestion(&s.id).unwrap().unwrap().status,
            "proposed"
        );
    }

    #[test]
    fn list_filters_orders_newest_first_and_counts_proposed() {
        let db = fresh_db();
        let a = db
            .propose_suggestion_at(&new("work_item", "a"), 10)
            .unwrap()
            .unwrap();
        let b = db
            .propose_suggestion_at(
                &NewSuggestion {
                    thread_id: Some("t2"),
                    message_id: None,
                    ..new("waiting_for_reply", "b")
                },
                20,
            )
            .unwrap()
            .unwrap();
        let c = db
            .propose_suggestion_at(&new("work_item", "c"), 20)
            .unwrap()
            .unwrap();
        let ids = |f: SuggestionFilter| -> Vec<String> {
            db.list_suggestions(&f)
                .unwrap()
                .into_iter()
                .map(|s| s.id)
                .collect()
        };
        // Newest first; the tie on created_at falls back to insertion order.
        assert_eq!(
            ids(Default::default()),
            vec![c.id.clone(), b.id.clone(), a.id.clone()]
        );
        assert_eq!(db.count_proposed_suggestions().unwrap(), 3);
        db.dismiss_suggestion_at(&a.id, 30).unwrap();
        assert_eq!(db.count_proposed_suggestions().unwrap(), 2);
        assert_eq!(
            ids(SuggestionFilter {
                status: Some("dismissed".into()),
                ..Default::default()
            }),
            vec![a.id.clone()]
        );
        assert_eq!(
            ids(SuggestionFilter {
                kind: Some("waiting_for_reply".into()),
                ..Default::default()
            }),
            vec![b.id.clone()]
        );
        assert_eq!(
            ids(SuggestionFilter {
                thread_id: Some("t1".into()),
                ..Default::default()
            }),
            vec![c.id.clone(), a.id.clone()]
        );
        assert_eq!(
            ids(SuggestionFilter {
                limit: Some(1),
                ..Default::default()
            }),
            vec![c.id.clone()]
        );
        // Project filter: via a project -> thread link, never otherwise.
        db.lock_conn()
            .execute(
                "INSERT INTO links (src_type, src_id, dst_type, dst_id, relation, origin,
                                    created_at, updated_at)
                 VALUES ('project', 'p1', 'thread', 't2', 'includes', 'manual', 1, 1)",
                [],
            )
            .unwrap();
        assert_eq!(
            ids(SuggestionFilter {
                project_id: Some("p1".into()),
                ..Default::default()
            }),
            vec![b.id.clone()]
        );
        assert!(db
            .list_suggestions(&SuggestionFilter {
                status: Some("bogus".into()),
                ..Default::default()
            })
            .is_err());
    }

    #[test]
    fn late_evidence_resolves_for_a_just_sent_message() {
        let db = fresh_db();
        // Proposed at send time: only the RFC822 id is known.
        let s = db
            .propose_suggestion_at(
                &NewSuggestion {
                    kind: "waiting_for_reply",
                    payload_json: r#"{"title":"Offer","account_id":"acc"}"#,
                    rfc822_message_id: Some("<own@x.test>"),
                    origin: "heuristic",
                    dedupe_key: "sent:1",
                    ..Default::default()
                },
                10,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            db.attach_sent_evidence("<own@x.test>", "seed_own", Some("t1"), 15)
                .unwrap(),
            1
        );
        assert_eq!(
            db.attach_sent_evidence("<own@x.test>", "seed_own", Some("t1"), 15)
                .unwrap(),
            0
        );
        let got = db.get_suggestion(&s.id).unwrap().unwrap();
        assert_eq!(
            (got.message_id.as_deref(), got.thread_id.as_deref()),
            (Some("seed_own"), Some("t1"))
        );
        // Accept resolves it too when attach never ran.
        let s2 = db
            .propose_suggestion_at(
                &NewSuggestion {
                    kind: "waiting_for_reply",
                    payload_json: r#"{"title":"Offer 2","account_id":"acc"}"#,
                    rfc822_message_id: Some("<own@x.test>"),
                    origin: "heuristic",
                    dedupe_key: "sent:2",
                    ..Default::default()
                },
                10,
            )
            .unwrap()
            .unwrap();
        db.accept_suggestion_at(&s2.id, 20).unwrap();
        assert_eq!(
            db.work_items_for(EntityKind::Thread, "t1").unwrap().len(),
            1
        );
    }

    #[test]
    fn accept_before_the_sent_copy_syncs_is_linked_when_it_arrives() {
        let db = fresh_db();
        let s = db
            .propose_suggestion_at(
                &NewSuggestion {
                    kind: "waiting_for_reply",
                    payload_json: r#"{"title":"Brand new conversation","account_id":"acc"}"#,
                    rfc822_message_id: Some("<late@x.test>"),
                    origin: "heuristic",
                    dedupe_key: "sent:new",
                    ..Default::default()
                },
                10,
            )
            .unwrap()
            .unwrap();
        // Accepted while the Sent copy is still unsynced: no thread link yet.
        let acc = db.accept_suggestion_at(&s.id, 20).unwrap();
        let item = acc.work_item.unwrap();
        assert!(db
            .work_items_for(EntityKind::Thread, "t1")
            .unwrap()
            .is_empty());
        let sync = |id: &str, uid: i64, from: &str, mailbox: &str| {
            db.lock_conn()
                .execute(
                    "INSERT INTO messages (id, account_id, mailbox_id, provider_uid,
                                           rfc822_message_id, thread_id, from_address)
                     VALUES (?1, 'acc', ?4, ?2, '<late@x.test>', 't1', ?3)",
                    params![id, uid, from, mailbox],
                )
                .unwrap();
        };
        sync("own1", 20, "me@local.test", "acc:Sent");
        // The Sent copy syncs: the accepted item gains the thread + message link.
        assert_eq!(
            db.attach_sent_evidence("<late@x.test>", "own1", Some("t1"), 30)
                .unwrap(),
            1
        );
        let linked = db.work_items_for(EntityKind::Thread, "t1").unwrap();
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].id, item.id);
        let got = db.get_suggestion(&s.id).unwrap().unwrap();
        assert_eq!(
            (got.message_id.as_deref(), got.thread_id.as_deref()),
            (Some("own1"), Some("t1"))
        );
        // Idempotent.
        assert_eq!(
            db.attach_sent_evidence("<late@x.test>", "own1", Some("t1"), 40)
                .unwrap(),
            0
        );
        assert_eq!(
            db.work_items_for(EntityKind::Thread, "t1").unwrap().len(),
            1
        );
    }

    #[test]
    fn a_foreign_sender_reusing_the_message_id_gets_no_evidence_or_links() {
        let db = fresh_db();
        let s = db
            .propose_suggestion_at(
                &NewSuggestion {
                    kind: "waiting_for_reply",
                    payload_json: r#"{"title":"Mine","account_id":"acc"}"#,
                    rfc822_message_id: Some("<late@x.test>"),
                    origin: "heuristic",
                    dedupe_key: "sent:spoof",
                    ..Default::default()
                },
                10,
            )
            .unwrap()
            .unwrap();
        db.accept_suggestion_at(&s.id, 20).unwrap();
        let sync = |id: &str, uid: i64, from: &str, mailbox: &str| {
            db.lock_conn()
                .execute(
                    "INSERT INTO messages (id, account_id, mailbox_id, provider_uid,
                                           rfc822_message_id, thread_id, from_address)
                     VALUES (?1, 'acc', ?4, ?2, '<late@x.test>', 't1', ?3)",
                    params![id, uid, from, mailbox],
                )
                .unwrap();
        };
        sync("foe1", 21, "evil@x.test", "acc:INBOX");
        // Forged From = the user's own address, but delivered to the INBOX.
        sync("forged1", 23, "me@local.test", "acc:INBOX");
        sync("own1", 22, "Me@Local.test", "acc:Sent");
        // A third party's message carrying the same Message-ID: ignored.
        assert_eq!(
            db.attach_sent_evidence("<late@x.test>", "foe1", Some("t1"), 30)
                .unwrap(),
            0
        );
        assert!(db
            .work_items_for(EntityKind::Thread, "t1")
            .unwrap()
            .is_empty());
        let got = db.get_suggestion(&s.id).unwrap().unwrap();
        assert_eq!((got.message_id, got.thread_id), (None, None));
        let prov: i64 = db
            .lock_conn()
            .query_row(
                "SELECT COUNT(*) FROM work_item_provenance WHERE source_id IN ('foe1', 'forged1')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(prov, 0);
        // Own Sent copy (differently-cased address): linked.
        assert_eq!(
            db.attach_sent_evidence("<late@x.test>", "own1", Some("t1"), 40)
                .unwrap(),
            1
        );
        assert_eq!(
            db.work_items_for(EntityKind::Thread, "t1").unwrap().len(),
            1
        );
    }

    #[test]
    fn another_accounts_sent_copy_and_legacy_rows_never_resolve_late() {
        let db = fresh_db();
        db.lock_conn()
            .execute_batch(
                "INSERT INTO accounts (id, email, display_name, provider)
                     VALUES ('acc2', 'me@other.test', 'Me2', 'imap');
                 INSERT INTO mailboxes (id, account_id, path, name, role)
                     VALUES ('acc2:Sent', 'acc2', 'Sent', 'Sent', 'sent');
                 INSERT INTO messages (id, account_id, mailbox_id, provider_uid,
                                       rfc822_message_id, thread_id, from_address)
                     VALUES ('x2', 'acc2', 'acc2:Sent', 1, '<late@x.test>', 't1', 'me@other.test');",
            )
            .unwrap();
        let mk = |payload: &str, key: &str| {
            let s = db
                .propose_suggestion_at(
                    &NewSuggestion {
                        kind: "waiting_for_reply",
                        payload_json: payload,
                        rfc822_message_id: Some("<late@x.test>"),
                        origin: "heuristic",
                        dedupe_key: key,
                        ..Default::default()
                    },
                    10,
                )
                .unwrap()
                .unwrap();
            db.accept_suggestion_at(&s.id, 20)
                .unwrap()
                .work_item
                .unwrap()
                .id
        };
        // One acc suggestion, one legacy one with no account_id.
        mk(r#"{"title":"A","account_id":"acc"}"#, "k1");
        mk(r#"{"title":"Legacy"}"#, "k2");
        // acc2's Sent copy shares the Message-ID: nothing links, no provenance.
        assert_eq!(
            db.attach_sent_evidence("<late@x.test>", "x2", Some("t1"), 30)
                .unwrap(),
            0
        );
        assert!(db
            .work_items_for(EntityKind::Thread, "t1")
            .unwrap()
            .is_empty());
        let prov: i64 = db
            .lock_conn()
            .query_row(
                "SELECT COUNT(*) FROM work_item_provenance WHERE source_id = 'x2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(prov, 0);
        // The real acc Sent copy links only the acc item (legacy never does)
        // and a re-run counts nothing.
        db.lock_conn()
            .execute(
                "INSERT INTO messages (id, account_id, mailbox_id, provider_uid,
                                       rfc822_message_id, thread_id, from_address)
                 VALUES ('x1', 'acc', 'acc:Sent', 9, '<late@x.test>', 't1', 'me@local.test')",
                [],
            )
            .unwrap();
        assert_eq!(
            db.attach_sent_evidence("<late@x.test>", "x1", Some("t1"), 40)
                .unwrap(),
            1
        );
        assert_eq!(
            db.work_items_for(EntityKind::Thread, "t1").unwrap().len(),
            1
        );
        assert_eq!(
            db.attach_sent_evidence("<late@x.test>", "x1", Some("t1"), 50)
                .unwrap(),
            0
        );
    }
}
