// Castellum - repository layer over the profile database.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Typed CRUD/query methods on `Database`. All access goes through the single
// serialized connection; multi-row writes run in a transaction. Repositories
// return `castellum-models` DTOs — service code never sees rusqlite rows.

use crate::search_query;
use crate::{Database, DbError};
use castellum_models::{
    Account, Address, AttachmentDescriptor, BodyHydrationScope, CalendarInvite,
    CalendarInviteAttendee, CalendarInviteDateTime, Draft, DraftAddress, DraftAttachmentMeta,
    Mailbox, MailboxBodyBacklog, MailboxBodyProgress, MailboxCounts, MailboxSelectionBlocker,
    MessageDetail, MessageHeader, NewDraft, NewMessage, OperationRecord, ResetAccountBlockers,
    SenderRemoteContentPolicy, SnoozedMessageHeader, SyncCursor, ThreadSummary, TrustResult,
    TrustSignals, UnsnoozedMessage,
};
use rusqlite::{params, params_from_iter, types::Value, OptionalExtension};
use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

/// Unix epoch seconds, clock-failure-safe (falls back to 0 rather than
/// panicking). Used for the undo-send window (`send_draft_delayed`) and its
/// crash-safety heartbeat (`touch_last_clean_drain`/`reconcile_undo_sends`).
fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Error returned by an enqueue-layer mutation refused because the account is
/// in evaluation (read-only) mode. Stable, credential-free, human-readable.
///
/// CROSS-LANGUAGE CONTRACT: the Swift UI classifies a refusal by matching the
/// leading `"evaluation mode:"` marker (`AccountWritePolicy.evaluationModePrefix`),
/// so it can offer "Enable Writes…" instead of showing a raw error. Changing
/// that prefix is a breaking change for the app —
/// `evaluation_mode_message_keeps_the_prefix_the_swift_ui_matches` pins it.
pub const EVALUATION_MODE_MSG: &str =
    "evaluation mode: no changes are made to the mail server (enable writes to act)";

/// Scan cap for [`Database::find_similar_messages`]'s brute-force candidate
/// scan (S53-UI). No SQLite vector index exists over `message_embeddings`
/// (S52 didn't add one — see `migrations/0031_message_embeddings.sql`), so
/// this is a full row-by-row cosine-similarity scan; capping it keeps it
/// "obviously fast, never visibly wrong" the same way `SMART_MAILBOX_COUNT_CAP`
/// bounds `smart_mailboxes.rs`' own capped scan (Assumption 8 there). 3000
/// candidates is a handful of `get_message_embeddings` chunks
/// (`EMBEDDINGS_QUERY_CHUNK` = 500 — 6 chunks worst case) and comfortably
/// under SQLite's ~32766 bound-variable ceiling by construction, so Task 1's
/// defensive chunking is never actually exercised past a single chunk by
/// this call site in practice; past this cap a real vector index is the
/// right fix, not a bigger constant.
pub const FIND_SIMILAR_SCAN_CAP: i64 = 3000;

/// Send Later (HP1) late-send grace, in seconds. A scheduled send whose time
/// passed while the app was not running by MORE than this is not delivered
/// silently: it moves to `needs_confirmation` and waits for the user. Within
/// it, the send goes out. A reversible product choice; see DECISIONS.md
/// (2026-10-01, Send Later late-send rule).
pub const LATE_SEND_GRACE_SECONDS: i64 = 600;

/// Stable marker prefixing `Database::reset_account`'s refusal when the
/// account has an unsent outbox draft or a non-terminal queued operation and
/// `force` was not passed. The Swift UI matches this prefix (like
/// `EVALUATION_MODE_MSG`) to show a specific "discard it too?" dialog instead
/// of a raw error. Pinned by
/// `reset_blocked_message_keeps_the_prefix_the_swift_ui_matches`.
pub(crate) const RESET_BLOCKED_MSG_PREFIX: &str = "reset blocked:";

/// Stable marker prefixing `Database::apply_mailbox_selection`'s refusal when
/// a folder being removed from the sync selection still has a non-terminal
/// operation referencing it and `force` was not passed ( follow-
/// up review). Same idiom as `RESET_BLOCKED_MSG_PREFIX`: the Swift UI matches
/// this prefix to offer "sync now and retry" / "discard and remove" instead of
/// a raw error. Pinned by
/// `mailbox_removal_blocked_message_keeps_the_prefix_the_swift_ui_matches`.
pub(crate) const MAILBOX_REMOVAL_BLOCKED_MSG_PREFIX: &str = "mailbox removal blocked:";

/// Why a read-flag change is being made. Evaluation mode treats the two
/// differently: an explicit toggle is a write like any other and is refused,
/// while auto-mark-on-open stays local so reading mail never requires leaving
/// read-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Intent {
    /// The user asked for this (Toggle Read/Unread).
    Explicit,
    /// A side effect of opening the message.
    Automatic,
}

/// Most UIDs one cancelled Empty Trash records for re-fetch (a huge Trash is
/// re-listed by the normal sync paths long before this matters).
/// Per-mailbox body-hydration progress (#3631 G5); see
/// [`Database::body_hydration_progress`]. A const so a test can EXPLAIN it.
const BODY_PROGRESS_SQL: &str = "SELECT mb.id,
        (SELECT COUNT(*) FROM messages m
          WHERE m.account_id = mb.account_id AND m.mailbox_id = mb.id
            AND m.provider_uid IS NOT NULL),
        (SELECT COUNT(*) FROM messages m
          JOIN message_bodies b ON b.message_id = m.id
          WHERE m.account_id = mb.account_id AND m.mailbox_id = mb.id
            AND m.provider_uid IS NOT NULL)
     FROM mailboxes mb
     WHERE mb.account_id = ?1
     ORDER BY mb.id";

/// SQL for [`Database::body_hydration_scope`]; a fn so a test can EXPLAIN
/// each of the three scope shapes. Placeholders are positional (`?`) in the
/// order account, mailbox.
fn body_scope_sql(by_account: bool, by_mailbox: bool, extra_where: &str) -> String {
    let mut sql = String::from(
        "SELECT COUNT(*), SUM(b.message_id IS NULL)
         FROM messages m
         LEFT JOIN message_bodies b ON b.message_id = m.id
         WHERE m.provider_uid IS NOT NULL",
    );
    if by_account {
        sql.push_str(" AND m.account_id = ?");
    }
    if by_mailbox {
        sql.push_str(" AND m.mailbox_id = ?");
    }
    if !extra_where.is_empty() {
        sql.push_str(" AND (");
        sql.push_str(extra_where);
        sql.push(')');
    }
    sql
}

/// Only the mailbox-targeting filters of a search query (`in:`, `folder:`,
/// `path:`, with their negation), as a filter-only [`search_query::ParsedQuery`].
fn mailbox_filters_only(query: &str) -> search_query::ParsedQuery {
    let mut parsed = search_query::parse(query);
    parsed.terms.clear();
    parsed.filters.retain(|f| {
        matches!(
            f.filter,
            search_query::Filter::In(_)
                | search_query::Filter::Folder(_)
                | search_query::Filter::Path(_)
        )
    });
    parsed
}

const REFETCH_PER_OP_CAP: usize = 5_000;

/// Queue the re-fetches an `undo_move` owes once it is terminal (succeeded,
/// failed for good, retired or cancelled). `undo_moves` deletes every local
/// copy of the message in the original destination up front, and sync never
/// re-imports a UID below UIDNEXT, so any copy the server still holds there
/// would stay hidden: the undo failed or was cancelled, a Gmail All Mail copy
/// keeps its UID after the label moves back, or a second genuine copy shared
/// the Message-ID. Re-fetch the recorded COPYUID and every dropped copy in the
/// destination, plus, when the original move itself failed for good, its UID
/// in the source (that undo "succeeds" without the message ever having left,
/// but the move had already dropped the source row).
/// UIDs the server no longer holds simply clear themselves on the next pass.
/// A no-op for any other op kind.
fn queue_undo_move_refetch(conn: &rusqlite::Connection, undo_op_id: i64) -> Result<(), DbError> {
    let row: Option<(String, String, String)> = conn
        .query_row(
            "SELECT account_id, kind, payload_json FROM operations WHERE id = ?1",
            params![undo_op_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((account, kind, payload_json)) = row else {
        return Ok(());
    };
    if kind != "undo_move" {
        return Ok(());
    }
    let json = |j: &str| -> serde_json::Value {
        serde_json::from_str(j).unwrap_or(serde_json::Value::Null)
    };
    let payload: serde_json::Value = json(&payload_json);
    let (original, original_status): (serde_json::Value, String) =
        match payload.get("undo_of").and_then(|v| v.as_i64()) {
            Some(original) => conn
                .query_row(
                    "SELECT payload_json, status FROM operations WHERE id = ?1",
                    params![original],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()?
                .map(|(j, st)| (json(&j), st))
                .unwrap_or((serde_json::Value::Null, String::new())),
            None => (serde_json::Value::Null, String::new()),
        };
    let int = |v: &serde_json::Value, key: &str| v.get(key).and_then(|x| x.as_i64());
    // Roles are swapped in the undo payload: its src is the original's dest.
    let mut targets: Vec<(&str, i64, i64)> = Vec::new();
    if let Some(dest) = payload.get("src_path").and_then(|v| v.as_str()) {
        if let (Some(uid), Some(validity)) = (
            int(&original, "dest_uid"),
            int(&original, "dest_uid_validity"),
        ) {
            targets.push((dest, uid, validity));
        }
        for pair in payload
            .get("dropped_dest")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            if let (Some(uid), Some(validity)) = (
                pair.get(0).and_then(|v| v.as_i64()),
                pair.get(1).and_then(|v| v.as_i64()),
            ) {
                targets.push((dest, uid, validity));
            }
        }
    }
    if let (true, Some(src), Some(uid), Some(validity)) = (
        original_status == "failed_permanent",
        payload.get("dest_path").and_then(|v| v.as_str()),
        int(&original, "uid"),
        int(&original, "uid_validity"),
    ) {
        targets.push((src, uid, validity));
    }
    let mut insert = conn.prepare_cached(
        "INSERT OR IGNORE INTO pending_refetch (account_id, mailbox_path, uid, uid_validity)
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    for (path, uid, validity) in targets {
        if uid > 0 && validity > 0 {
            insert.execute(params![account, path, uid, validity])?;
        }
    }
    Ok(())
}

/// Expand a compact IMAP UID set (`"3,5:7,10"`, as `compact_uid_ranges`
/// writes it) into UIDs, at most `cap`. Total: junk tokens are skipped.
fn expand_uid_ranges(set: &str, cap: usize) -> Vec<i64> {
    let mut out = Vec::new();
    for token in set.split(',') {
        let (lo, hi) = match token.split_once(':') {
            Some((a, b)) => (a.trim().parse::<i64>(), b.trim().parse::<i64>()),
            None => (token.trim().parse::<i64>(), token.trim().parse::<i64>()),
        };
        let (Ok(lo), Ok(hi)) = (lo, hi) else { continue };
        for uid in lo.max(1)..=hi.min(lo.saturating_add(cap as i64)) {
            if out.len() >= cap {
                return out;
            }
            out.push(uid);
        }
    }
    out
}

/// Where [`Database::move_messages`] sends a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveTarget<'a> {
    /// The account's role-`archive` mailbox.
    Archive,
    /// The account's role-`trash` mailbox (a message already there is refused).
    Trash,
    /// A specific mailbox, by id (must belong to the message's account).
    Mailbox(&'a str),
}

/// One message [`Database::move_messages`] declined to move.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MoveRefusal {
    pub message_id: String,
    pub reason: String,
    /// True when the account is read-only (evaluation mode), so the UI can
    /// offer "Enable Writes" instead of a raw error.
    pub evaluation_mode: bool,
}

/// Result of [`Database::move_messages`].
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct MoveBatchOutcome {
    /// One queued `move` op id per message actually moved, in input order.
    /// Opaque undo handles for [`Database::undo_moves`]. A message already in
    /// the destination queues nothing and contributes no id.
    pub operation_ids: Vec<i64>,
    pub refused: Vec<MoveRefusal>,
}

/// One op [`Database::undo_moves`] could not undo.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct UndoRefusal {
    pub operation_id: i64,
    pub reason: String,
    pub evaluation_mode: bool,
}

/// Result of [`Database::undo_moves`].
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct UndoMoveOutcome {
    /// `undo_move` ops inserted.
    pub queued: usize,
    pub not_undoable: Vec<UndoRefusal>,
    pub account_ids: Vec<String>,
    /// Mailboxes (by id) the mail is returning to.
    pub source_mailbox_ids: Vec<String>,
}

/// SQL predicate (over the `messages` table, unaliased) that is true for a
/// message whose Message-ID is on its way back out of Trash/Archive through a
/// pending `undo_move`. Empty Trash must neither expunge nor locally delete
/// such a row (undo guard G2): the server still holds the only copy.
const UNDO_IN_FLIGHT_SQL: &str = "EXISTS (
        SELECT 1 FROM operations uo
         WHERE uo.account_id = messages.account_id
           AND uo.kind = 'undo_move'
           AND uo.status IN ('queued', 'running', 'failed')
           AND COALESCE(messages.rfc822_message_id, '') <> ''
           AND json_extract(uo.payload_json, '$.message_id') = messages.rfc822_message_id)";

/// Read the per-account write policy from an already-held connection (so the
/// enqueue guards can consult it inside their own transaction without a
/// re-entrant lock). FAIL-CLOSED: missing row or stored 0 ⇒ read-only.
pub(crate) fn write_enabled_on(
    conn: &rusqlite::Connection,
    account_id: &str,
) -> rusqlite::Result<bool> {
    let enabled: Option<i64> = conn
        .query_row(
            "SELECT write_enabled FROM accounts WHERE id = ?1",
            params![account_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(enabled == Some(1))
}

/// Invalidate a mailbox's QRESYNC baseline after a LOCAL-ONLY flag change —
/// one that diverges local state from the server while the server itself
/// stays put (an evaluation-mode auto-read that queues no op, or a cancelled
/// op whose local flag was already applied). QRESYNC reports only what the
/// server changed, so it can never see such a divergence; before S30 the full
/// scan on every sync reverted it. Forcing the next sync onto the full scan
/// restores that.
///
/// The new value is a FRESH non-positive number, never NULL: non-positive is
/// already "not QRESYNC-eligible" to the sync engine, and each invalidation
/// yields a value no sync can have read before (a positive modseq or NULL
/// becomes -1, a non-positive v becomes v-1), so a sync already running when
/// it lands sees the change through `set_sync_cursor_if_modseq_is` and keeps
/// it instead of overwriting it with the modseq it was about to persist.
pub(crate) fn invalidate_mailbox_modseq(
    conn: &rusqlite::Connection,
    mailbox_id: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE sync_cursors
         SET highest_modseq = CASE WHEN highest_modseq > 0 THEN -1
                                   ELSE COALESCE(highest_modseq, 0) - 1 END,
             updated_at = datetime('now')
         WHERE mailbox_id = ?1",
        params![mailbox_id],
    )?;
    Ok(())
}

/// A test-only hook fired at every point where a thread rebuild has let go
/// of the connection mutex between two lock holds -- the only moments a
/// concurrent writer (or a second rebuild) can interleave with it. Tests
/// install a closure here to perform a write, start a competing rebuild, or
/// check a reader-visible invariant at exactly those points, which turns
/// review findings that otherwise need a lucky thread schedule to reproduce
/// into deterministic tests. Thread-local, so it only fires on the thread
/// that installed it; compiled out of non-test builds entirely.
#[cfg(test)]
type RebuildGapHook = Box<dyn FnMut(&Database, &'static str)>;

#[cfg(test)]
thread_local! {
    static REBUILD_GAP_HOOK: std::cell::RefCell<Option<RebuildGapHook>> =
        const { std::cell::RefCell::new(None) };
}

/// Fire [`REBUILD_GAP_HOOK`] (tests only; a no-op otherwise). The hook is
/// taken out of its slot while it runs, so a hook that itself calls into a
/// rebuild cannot re-enter itself.
#[cfg_attr(not(test), inline(always))]
fn rebuild_gap(db: &Database, phase: &'static str) {
    #[cfg(test)]
    {
        let hook = REBUILD_GAP_HOOK.with(|h| h.borrow_mut().take());
        if let Some(mut hook) = hook {
            hook(db, phase);
            REBUILD_GAP_HOOK.with(|h| {
                let mut slot = h.borrow_mut();
                if slot.is_none() {
                    *slot = Some(hook);
                }
            });
        }
    }
    #[cfg(not(test))]
    let _ = (db, phase);
}

/// Max fraction of an account's messages that may be dirty before
/// [`Database::rebuild_threads_incremental`] gives up and falls back to a
/// full [`Database::rebuild_threads`]: a full rebuild is
/// O(messages) either way, so past this point recomputing "just the
/// affected threads" stops saving anything over starting clean, and the
/// closure walk itself risks costing as much as the scan it was meant to
/// avoid.
const INCREMENTAL_DIRTY_FRACTION_LIMIT: f64 = 0.20;

/// Hard ceiling on the affected-thread closure one closure walk of an
/// incremental rebuild will chase before giving up and falling back to a
/// full rebuild -- guards
/// against one pathologically huge thread (a mailing list) turning a single
/// dirty message into an account-sized recompute anyway, which the dirty-
/// fraction check alone would miss (the dirty SET can be tiny while its
/// CLOSURE is not).
const INCREMENTAL_CLOSURE_LIMIT: usize = 20_000;

/// Target messages per chunk transaction while an incremental rebuild
/// recomputes its closure -- bounds how long any ONE transaction (and so
/// the DB mutex every UI read also waits on; see `Database::lock_conn`)
/// stays held, however large the whole closure turns out to be. Chosen to
/// land near the ~20ms/chunk budget the plan asks for, in the
/// same spirit as `DESCRIPTOR_UPSERT_BATCH_SIZE` (castellum-core::sync). A
/// chunk only ever holds WHOLE connected groups (PR #181 review), so one
/// group bigger than this is a chunk of its own rather than being split.
const INCREMENTAL_WRITE_CHUNK: usize = 500;

/// Dirty seeds expanded together per closure walk while filling a chunk: a
/// walk costs a handful of indexed queries per round however many seeds it
/// starts from, so batching keeps a typical follow-up sync (tens of dirty
/// messages) to one walk, while a chunk still stops growing soon after it
/// reaches `INCREMENTAL_WRITE_CHUNK` members.
const INCREMENTAL_SEED_BATCH: usize = 64;

/// Bound-parameter count per `IN (...)` query chunk while walking a thread
/// closure or writing it back -- comfortably under SQLite's default
/// `SQLITE_LIMIT_VARIABLE_NUMBER`, and small enough that no single query
/// dominates a chunk's time budget.
const IN_CLAUSE_CHUNK: usize = 400;

/// Mark an account's thread projection dirty AND record exactly which
/// message(s) a write just touched: every write path that
/// can change thread membership or an aggregate `rebuild_threads` derives
/// calls this with the message id(s) it just touched, in the SAME
/// transaction as the mutation. [`Database::rebuild_threads_incremental`]
/// reads `thread_dirty_messages` (migration 0039) to recompute only the
/// threads those messages' closure touches, instead of the whole account;
/// both it and a full [`Database::rebuild_threads`] clear the rows they
/// process. The boolean `thread_rebuild_state.dirty` (migration 0037) is
/// kept too -- [`Database::threads_dirty`]'s existing callers (`sync.rs`'s
/// skip-rebuild check) still just need "is anything stale", not the set
/// itself. Always called inside the SAME transaction as the mutation it
/// follows, so a mid-transaction rollback (e.g. an evaluation-mode refusal
/// earlier in the same call) never records a change that didn't happen.
pub(crate) fn mark_threads_dirty(
    tx: &rusqlite::Transaction<'_>,
    account_id: &str,
    message_ids: &[&str],
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO thread_rebuild_state (account_id, dirty) VALUES (?1, 1)
         ON CONFLICT(account_id) DO UPDATE SET dirty = 1",
        params![account_id],
    )?;
    // A message that is already dirty gets its `gen` bumped rather than the
    // write being ignored (PR #181 review, finding 3): an incremental
    // rebuild clears a row only while its gen still matches the snapshot it
    // took, so this write can never be cleared by a pass that did not see it.
    let mut stmt = tx.prepare_cached(
        "INSERT INTO thread_dirty_messages (account_id, message_id) VALUES (?1, ?2)
         ON CONFLICT(account_id, message_id) DO UPDATE SET gen = gen + 1",
    )?;
    for id in message_ids {
        stmt.execute(params![account_id, id])?;
    }
    Ok(())
}

/// [`mark_threads_dirty`] for a mutation too broad to name individual
/// messages -- a mailbox purge (`delete_messages_for_mailbox`, the legacy
/// `clear_sync_cursor`/empty-trash-recovery purge) or a UIDVALIDITY reset.
/// Sets `thread_rebuild_state.force_full` (migration 0039) instead of
/// populating `thread_dirty_messages`: `rebuild_threads_incremental` always
/// takes the full-rebuild fallback while it is set, both rebuild paths
/// clear it, and enumerating potentially thousands of purged ids would blow
/// the incremental dirty-fraction budget anyway.
fn mark_threads_dirty_full(
    tx: &rusqlite::Transaction<'_>,
    account_id: &str,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO thread_rebuild_state (account_id, dirty, force_full) VALUES (?1, 1, 1)
         ON CONFLICT(account_id) DO UPDATE SET dirty = 1, force_full = 1",
        params![account_id],
    )?;
    Ok(())
}

/// Run BEFORE deleting `departing` (message ids about to leave `messages`
/// entirely -- moved away, expunged, vanished; never a flag change). Seeds
/// every SURVIVING member of the threads they belong to as dirty, so the
/// incremental rebuild recomputes those threads without them (count,
/// unread, root and so id), and drops the departing ids' own dirty rows
/// (nothing is left to recompute for a message that is gone). Returns the
/// departing messages' thread ids: after the DELETE, the caller hands them
/// to [`sweep_emptied_threads`], which removes any that now have no member.
///
/// Seeding works on the whole departing set at once. The previous
/// per-message version looked for "siblings" one message at
/// a time while the others were still present, so a thread whose members
/// ALL left together (Empty Trash on a deleted conversation -- PR #181
/// review, finding 1) had each member seed the other, no member saw an
/// empty sibling list, and after the bulk DELETE the seeds named nothing:
/// the thread row survived as a phantom conversation.
fn prepare_message_departures(
    tx: &rusqlite::Transaction<'_>,
    account_id: &str,
    departing: &[String],
) -> rusqlite::Result<Vec<String>> {
    let thread_ids = load_thread_ids_for_message_ids(tx, account_id, departing)?;
    let departing_set: HashSet<&str> = departing.iter().map(String::as_str).collect();
    let mut survivors: Vec<String> = Vec::new();
    for chunk in thread_ids.chunks(IN_CLAUSE_CHUNK) {
        let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT id FROM messages INDEXED BY idx_messages_thread
             WHERE account_id = ? AND thread_id IN ({placeholders})"
        );
        let mut stmt = tx.prepare(&sql)?;
        let p =
            params_from_iter(std::iter::once(account_id).chain(chunk.iter().map(String::as_str)));
        for id in stmt.query_map(p, |r| r.get::<_, String>(0))? {
            let id = id?;
            if !departing_set.contains(id.as_str()) {
                survivors.push(id);
            }
        }
    }
    if !survivors.is_empty() {
        let refs: Vec<&str> = survivors.iter().map(String::as_str).collect();
        mark_threads_dirty(tx, account_id, &refs)?;
    }
    let mut drop_dirty = tx.prepare_cached(
        "DELETE FROM thread_dirty_messages WHERE account_id = ?1 AND message_id = ?2",
    )?;
    for id in departing {
        drop_dirty.execute(params![account_id, id])?;
    }
    Ok(thread_ids)
}

/// Delete every `threads` row among `candidates` that no message points at
/// any more -- run AFTER messages left (or were re-pointed) in the same
/// transaction, so no reader ever sees a conversation with no messages.
/// `idx_messages_thread` makes each check one index probe, so the cost
/// scales with the candidates, never with the account. Used by every path
/// that removes messages and by both rebuild paths for thread ids that
/// genuinely disappeared.
fn sweep_emptied_threads(
    tx: &rusqlite::Transaction<'_>,
    account_id: &str,
    candidates: &[String],
) -> rusqlite::Result<usize> {
    let mut removed = 0;
    for chunk in candidates.chunks(IN_CLAUSE_CHUNK) {
        let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "DELETE FROM threads
             WHERE account_id = ? AND id IN ({placeholders})
               AND NOT EXISTS (
                   SELECT 1 FROM messages m INDEXED BY idx_messages_thread
                   WHERE m.thread_id = threads.id)"
        );
        let p =
            params_from_iter(std::iter::once(account_id).chain(chunk.iter().map(String::as_str)));
        removed += tx.execute(&sql, p)?;
    }
    Ok(removed)
}

/// Drop AI state keyed by message ids that are about to be deleted.
/// `ai_outputs.owner_id` and the `message_id` inside `ai_jobs.payload_json`
/// carry no FK, and ids are `<mailbox>:<uid>`, so after a UIDVALIDITY reset
/// the refetch reuses them for different mail: a stale cached summary would
/// show on the wrong message and a queued job would run against it. `ids_sql`
/// selects the doomed ids (run BEFORE the messages are deleted).
fn purge_ai_for_messages(
    tx: &rusqlite::Transaction<'_>,
    ids_sql: &str,
    params: &[&dyn rusqlite::ToSql],
) -> rusqlite::Result<()> {
    tx.execute(
        &format!("DELETE FROM ai_outputs WHERE scope = 'message' AND owner_id IN ({ids_sql})"),
        params,
    )?;
    // Jobs the executor holds right now (leased/running/streaming) must not
    // vanish under it: its later event/status writes would hit a missing row
    // and abort the whole pass. Flag them instead; the executor's
    // post-generation check discards the output and lands them on cancelled.
    tx.execute(
        &format!(
            "UPDATE ai_jobs SET cancel_requested = 1, updated_at = datetime('now')
             WHERE status IN ('leased', 'running', 'streaming')
               AND json_extract(payload_json, '$.message_id') IN ({ids_sql})"
        ),
        params,
    )?;
    tx.execute(
        &format!(
            "DELETE FROM ai_jobs
             WHERE status NOT IN ('succeeded', 'cancelled', 'leased', 'running', 'streaming')
               AND json_extract(payload_json, '$.message_id') IN ({ids_sql})"
        ),
        params,
    )?;
    Ok(())
}

/// Thread-scope outputs whose thread no longer exists (after the sweep).
fn purge_orphan_thread_ai_outputs(tx: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute(
        "DELETE FROM ai_outputs WHERE scope = 'thread'
           AND owner_id NOT IN (SELECT id FROM threads)",
        [],
    )?;
    Ok(())
}

/// Delete every message of one mailbox (a UIDVALIDITY reset, an
/// interrupted first import, the legacy empty-trash recovery) and keep the
/// thread projection honest in the same transaction: threads the purge
/// emptied are swept at once (PR #181 review, finding 1 -- no phantom
/// conversation until the next rebuild), and `force_full` sends the next
/// rebuild down the full path, since the survivors of every partly-purged
/// thread are not worth enumerating for a purge this broad. Returns the
/// number of messages deleted.
fn purge_mailbox_messages(
    tx: &rusqlite::Transaction<'_>,
    account_id: &str,
    mailbox_id: &str,
) -> rusqlite::Result<usize> {
    let thread_ids: Vec<String> = tx
        .prepare_cached(
            "SELECT DISTINCT thread_id FROM messages
             WHERE account_id = ?1 AND mailbox_id = ?2 AND thread_id IS NOT NULL",
        )?
        .query_map(params![account_id, mailbox_id], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    purge_ai_for_messages(
        tx,
        "SELECT id FROM messages WHERE account_id = ?1 AND mailbox_id = ?2",
        params![account_id, mailbox_id],
    )?;
    let deleted = tx.execute(
        "DELETE FROM messages WHERE account_id = ?1 AND mailbox_id = ?2",
        params![account_id, mailbox_id],
    )?;
    if deleted > 0 {
        sweep_emptied_threads(tx, account_id, &thread_ids)?;
        purge_orphan_thread_ai_outputs(tx)?;
        mark_threads_dirty_full(tx, account_id)?;
    }
    Ok(deleted)
}

/// What [`Database::remove_mailboxes_gone_from_server`] removed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RemovedMailboxes {
    /// Stored paths of the mailboxes removed.
    pub paths: Vec<String>,
    /// Messages deleted with them.
    pub messages: usize,
    /// Queued operations cancelled because their folder is gone.
    pub cancelled_ops: usize,
    /// Attachment content hashes no remaining message references; the
    /// caller unlinks the cached files.
    pub orphaned_hashes: Vec<String>,
}

/// Purge ONE mailbox inside the caller's transaction: collect the
/// attachment content hashes no other mailbox references (before the
/// messages go), delete its messages through [`purge_mailbox_messages`],
/// drop its cursor and decode-skip retry rows, and — when `delete_row` —
/// the mailbox row itself. Shared by the folder-selection edit
/// (`apply_mailbox_selection`) and the server-side removal
/// (`remove_mailboxes_gone_from_server`) so the two cannot drift. Returns
/// the number of messages deleted.
fn purge_mailbox_in_tx(
    tx: &rusqlite::Transaction<'_>,
    account_id: &str,
    mailbox_id: &str,
    delete_row: bool,
    orphaned_hashes: &mut Vec<String>,
) -> rusqlite::Result<usize> {
    {
        let mut stmt = tx.prepare(
            "SELECT DISTINCT ac.content_hash FROM attachment_cache ac
               JOIN messages m ON m.id = ac.message_id
              WHERE m.mailbox_id = ?1
                AND NOT EXISTS (
                    SELECT 1 FROM attachment_cache ac2
                      JOIN messages m2 ON m2.id = ac2.message_id
                     WHERE ac2.content_hash = ac.content_hash
                       AND m2.mailbox_id <> ?1)",
        )?;
        let rows = stmt.query_map(params![mailbox_id], |r| r.get::<_, String>(0))?;
        for hash in rows {
            orphaned_hashes.push(hash?);
        }
    }
    // Messages first, through the shared purge (sweeps emptied threads,
    // sets force_full).
    let deleted = purge_mailbox_messages(tx, account_id, mailbox_id)?;
    tx.execute(
        "DELETE FROM sync_cursors WHERE account_id = ?1 AND mailbox_id = ?2",
        params![account_id, mailbox_id],
    )?;
    tx.execute(
        "DELETE FROM message_decode_skips WHERE account_id = ?1 AND mailbox_id = ?2",
        params![account_id, mailbox_id],
    )?;
    if delete_row {
        tx.execute("DELETE FROM mailboxes WHERE id = ?1", params![mailbox_id])?;
    }
    Ok(deleted)
}

/// The stable integer `message_fts` rows for `message_id` are keyed by
///, allocating one on first use. A PK lookup
/// (`message_fts_rowid.message_id` is the table's UNIQUE column) — O(log n),
/// never a scan. Never `messages.rowid`: `messages` has a TEXT primary key,
/// so its implicit rowid is an ordinary one VACUUM can renumber.
pub(crate) fn ensure_fts_rowid(
    tx: &rusqlite::Transaction<'_>,
    message_id: &str,
) -> rusqlite::Result<i64> {
    if let Some(id) = tx
        .query_row(
            "SELECT fts_rowid FROM message_fts_rowid WHERE message_id = ?1",
            params![message_id],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
    {
        return Ok(id);
    }
    tx.execute(
        "INSERT INTO message_fts_rowid (message_id) VALUES (?1)",
        params![message_id],
    )?;
    Ok(tx.last_insert_rowid())
}

fn trust_result_str(r: TrustResult) -> &'static str {
    match r {
        TrustResult::Pass => "pass",
        TrustResult::Fail => "fail",
        TrustResult::None => "none",
    }
}

fn trust_result_from_str(s: &str) -> TrustResult {
    match s {
        "pass" => TrustResult::Pass,
        "fail" => TrustResult::Fail,
        _ => TrustResult::None,
    }
}

/// Flatten one optional `CalendarInviteDateTime` into `calendar_invites`'
/// four columns for a `dtstart_*`/`dtend_*` pair. `None` becomes an absent
/// raw value and `0`/`0` flags, never NULL flags — keeps the row's booleans
/// always-defined regardless of whether the date itself is present.
fn datetime_columns(dt: Option<&CalendarInviteDateTime>) -> (Option<&str>, Option<&str>, i64, i64) {
    match dt {
        Some(dt) => (
            Some(dt.raw.as_str()),
            dt.tzid.as_deref(),
            i64::from(dt.is_utc),
            i64::from(dt.is_date_only),
        ),
        None => (None, None, 0, 0),
    }
}

/// One cached attachment's content address (`attachment_cache_entry`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentCacheEntry {
    pub content_hash: String,
    pub size_bytes: i64,
}

/// One distinct cached hash's aggregate usage (`attachment_cache_hashes_by_lru`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentCacheHashUsage {
    pub content_hash: String,
    pub size_bytes: i64,
    pub last_accessed_epoch: i64,
}

impl Database {
    // --- accounts ---------------------------------------------------------

    /// Insert or update an account (by id).
    pub fn upsert_account(&self, a: &Account) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO accounts (id, email, display_name, provider)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET
                 email = excluded.email,
                 display_name = excluded.display_name,
                 provider = excluded.provider",
            params![a.id, a.email, a.display_name, a.provider],
        )?;
        Ok(())
    }

    /// Whether the account may make changes to the mail server. FAIL-CLOSED:
    /// an unknown account (no row) or a stored 0 is read-only ("evaluation
    /// mode"); only an explicit stored 1 permits writes. This is the single
    /// source of truth consulted by the replay/send choke points and the
    /// enqueue-layer guards.
    pub fn account_write_enabled(&self, account_id: &str) -> Result<bool, DbError> {
        let conn = self.lock_conn();
        write_enabled_on(&conn, account_id).map_err(DbError::from)
    }

    /// Enable or disable writes for an account (leave evaluation mode). Errors
    /// when the account does not exist so a UI toggle cannot silently no-op.
    pub fn set_account_write_enabled(
        &self,
        account_id: &str,
        enabled: bool,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        let n = conn.execute(
            "UPDATE accounts SET write_enabled = ?2 WHERE id = ?1",
            params![account_id, i64::from(enabled)],
        )?;
        if n == 0 {
            return Err(DbError(format!("unknown account id: {account_id}")));
        }
        Ok(())
    }

    /// List all accounts, ordered by email.
    pub fn list_accounts(&self) -> Result<Vec<Account>, DbError> {
        let conn = self.lock_conn();
        let mut stmt =
            conn.prepare("SELECT id, email, display_name, provider FROM accounts ORDER BY email")?;
        let rows = stmt.query_map([], |r| {
            Ok(Account {
                id: r.get(0)?,
                email: r.get(1)?,
                display_name: r.get(2)?,
                provider: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Permanently delete an account and every locally-stored mail record it
    /// owns: mailboxes, messages (with addresses/flags/bodies), sync cursors,
    /// and queued operations cascade via `ON DELETE CASCADE` on the `accounts`
    /// row. `drafts.account_id` carries no foreign key (see migration 0007),
    /// so its rows are deleted explicitly in the same transaction to avoid
    /// leaving them orphaned. Deliberately NOT touched: `account_sync_state`
    /// (migration 0016 documents it as designed to outlive a deleted account)
    /// and `ai_outputs`/`ai_jobs` (outlive their source entities by design,
    /// see migration 0010). Errors when the account does not exist so a UI
    /// Remove action cannot silently no-op. Credential removal from the
    /// Keychain is the Swift side's job (`AccountStore.delete`) — this only
    /// ever touches the local SQLite profile.
    pub fn delete_account(&self, account_id: &str) -> Result<(), DbError> {
        self.delete_account_collecting(account_id).map(|_| ())
    }

    /// [`Self::delete_account`], also returning what the core layer must
    /// unlink from disk after the commit (#3364): the attachment content
    /// hashes only this account referenced, and the account's message ids
    /// (a cached raw `.eml` is named by a hash of its message id). Both must
    /// be read BEFORE the cascade; unlinking a missing file is harmless.
    pub fn delete_account_collecting(
        &self,
        account_id: &str,
    ) -> Result<(Vec<String>, Vec<String>), DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let hashes = orphaned_attachment_hashes(&tx, account_id)?;
        let message_ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM messages WHERE account_id = ?1")?;
            let rows = stmt.query_map(params![account_id], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        tx.execute(
            "DELETE FROM drafts WHERE account_id = ?1",
            params![account_id],
        )?;
        let n = tx.execute("DELETE FROM accounts WHERE id = ?1", params![account_id])?;
        if n == 0 {
            return Err(DbError(format!("unknown account id: {account_id}")));
        }
        tx.commit()?;
        Ok((hashes, message_ids))
    }

    /// Every content hash any `attachment_cache` row still references (the
    /// live set for the on-disk orphan sweep, #3364).
    pub fn attachment_cache_live_hashes(
        &self,
    ) -> Result<std::collections::HashSet<String>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare("SELECT DISTINCT content_hash FROM attachment_cache")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Ids of every message in the profile (the live set for the raw-source
    /// cache sweep, #3364).
    pub fn all_message_ids(&self) -> Result<Vec<String>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare("SELECT id FROM messages")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// What an unforced `reset_account` would refuse on: unsent outbox
    /// drafts and non-terminal queued operations for this account. A read —
    /// callers use it to word the confirmation dialog before deciding
    /// whether to pass `force`. `reset_account` itself re-checks the same
    /// counts inside its own transaction, so this is advisory, not a lock.
    pub fn reset_account_blockers(
        &self,
        account_id: &str,
    ) -> Result<ResetAccountBlockers, DbError> {
        let conn = self.lock_conn();
        let unsent_outbox_drafts: i64 = conn.query_row(
            "SELECT COUNT(*) FROM drafts WHERE account_id = ?1 AND status = 'outbox'",
            params![account_id],
            |r| r.get(0),
        )?;
        let pending_operations: i64 = conn.query_row(
            "SELECT COUNT(*) FROM operations WHERE account_id = ?1 AND status NOT IN ('succeeded', 'cancelled')",
            params![account_id],
            |r| r.get(0),
        )?;
        Ok(ResetAccountBlockers {
            unsent_outbox_drafts,
            pending_operations,
        })
    }

    ///: wipe an account's local mail state back to empty,
    /// keeping its row (unlike `delete_account`), its Keychain credentials
    /// (untouched — this never runs Swift/AccountStore code), its
    /// `write_enabled` policy, and every cross-account derived table
    /// (`contacts`/`contact_edges`/`contact_enrichment`). The caller triggers
    /// a fresh sync afterwards.
    ///
    /// Refuses (returning an error prefixed `RESET_BLOCKED_MSG_PREFIX`) when
    /// the account has an un-delivered outbox draft or any operation not yet
    /// `succeeded`/`cancelled`, UNLESS `force` is true — in which case those
    /// rows are discarded too. That is crash-safe: an in-flight operation's
    /// own later status-update (`Database::set_operation_status`) finds no row
    /// and returns an ordinary `DbError` for the unknown id — never a panic,
    /// never a poisoned lock (pinned by
    /// `reset_account_force_leaves_an_in_flight_operation_update_a_clean_error`).
    /// Returns the content hashes of cached attachment files no other account
    /// references; the caller must delete those files (the rows are gone).
    /// Any failure part-way through rolls the whole transaction back. Always re-checks the counts inside its OWN transaction
    /// (never trusts an earlier `reset_account_blockers` read), exactly like
    /// `send_draft_delayed`'s in-transaction `write_enabled_on` check.
    ///
    /// Deliberately NOT touched: `accounts` (the row itself), `contacts`/
    /// `contact_edges`/`contact_enrichment` (cross-account derived data —
    /// refresh separately, e.g. `AppModel.refreshContactsNow()`),
    /// `account_sync_state` (diagnostic history the next sync attempt
    /// overwrites), `accounts.write_enabled` (a policy setting).
    pub fn reset_account(&self, account_id: &str, force: bool) -> Result<Vec<String>, DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;

        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ?1)",
            params![account_id],
            |r| r.get(0),
        )?;
        if !exists {
            return Err(DbError(format!("unknown account id: {account_id}")));
        }

        if !force {
            let unsent_outbox_drafts: i64 = tx.query_row(
                "SELECT COUNT(*) FROM drafts WHERE account_id = ?1 AND status = 'outbox'",
                params![account_id],
                |r| r.get(0),
            )?;
            let pending_operations: i64 = tx.query_row(
                "SELECT COUNT(*) FROM operations WHERE account_id = ?1 AND status NOT IN ('succeeded', 'cancelled')",
                params![account_id],
                |r| r.get(0),
            )?;
            if unsent_outbox_drafts > 0 || pending_operations > 0 {
                return Err(DbError(format!(
                    "{RESET_BLOCKED_MSG_PREFIX} {unsent_outbox_drafts} unsent outbox draft(s), \
                     {pending_operations} pending operation(s)"
                )));
            }
        }

        // AI artefacts referencing this account's messages/threads must go
        // BEFORE those rows disappear — ai_outputs.owner_id carries no FK.
        tx.execute(
            "DELETE FROM ai_outputs WHERE scope = 'message'
               AND owner_id IN (SELECT id FROM messages WHERE account_id = ?1)",
            params![account_id],
        )?;
        tx.execute(
            "DELETE FROM ai_outputs WHERE scope = 'thread'
               AND owner_id IN (SELECT id FROM threads WHERE account_id = ?1)",
            params![account_id],
        )?;
        tx.execute(
            "DELETE FROM ai_jobs WHERE account_id = ?1",
            params![account_id],
        )?;

        // Threads carry no delete trigger for `links` (migration 0017: a
        // thread is "virtual", its links live until explicitly unlinked) —
        // purge their link rows BEFORE the threads go.
        tx.execute(
            "DELETE FROM links
             WHERE (src_type = 'thread' AND src_id IN (SELECT id FROM threads WHERE account_id = ?1))
                OR (dst_type = 'thread' AND dst_id IN (SELECT id FROM threads WHERE account_id = ?1))",
            params![account_id],
        )?;
        tx.execute(
            "DELETE FROM threads WHERE account_id = ?1",
            params![account_id],
        )?;
        // Every message goes below and every thread just went: nothing is
        // stale any more, so no rebuild marker may outlive the reset (a
        // dirty row naming a message that no longer exists would only count
        // against the next incremental rebuild's dirty-fraction budget).
        tx.execute(
            "DELETE FROM thread_dirty_messages WHERE account_id = ?1",
            params![account_id],
        )?;
        tx.execute(
            "DELETE FROM thread_rebuild_state WHERE account_id = ?1",
            params![account_id],
        )?;

        // Downloaded attachment BYTES live on disk, one file per content hash
        // (castellum-core `attachments::cached_file_path`). The
        // `attachment_cache` rows cascade away with `messages`, after which
        // the LRU evictor can no longer see the files — so collect the
        // hashes NO OTHER account still references BEFORE the cascade and
        // hand them back for the core layer to unlink after the commit.
        let orphaned_hashes = orphaned_attachment_hashes(&tx, account_id)?;

        // Mailboxes cascade: messages -> message_addresses/flags/bodies/
        // attachments (ON DELETE CASCADE, migration 0002/0014) and
        // message_fts (messages_before_delete_fts trigger, migration 0038)
        // and `links` src/dst_type='message' rows (links_message_delete
        // trigger, migration 0017); sync_cursors and mailbox_sync_state
        // cascade too (both FK mailboxes(id) ON DELETE CASCADE, migrations
        // 0002/0016), as do `attachment_cache` rows (migration 0022) and
        // message_attachments (migration 0014, metadata only).
        tx.execute(
            "DELETE FROM mailboxes WHERE account_id = ?1",
            params![account_id],
        )?;

        // Drafts carry no FK (see `delete_account`); operations cascade
        // their blobs (operation_blobs FK operations(id) ON DELETE CASCADE,
        // migration 0015).
        tx.execute(
            "DELETE FROM ai_outputs WHERE scope = 'draft'
               AND owner_id IN (SELECT CAST(id AS TEXT) FROM drafts WHERE account_id = ?1)",
            params![account_id],
        )?;
        tx.execute(
            "DELETE FROM drafts WHERE account_id = ?1",
            params![account_id],
        )?;
        tx.execute(
            "DELETE FROM operations WHERE account_id = ?1",
            params![account_id],
        )?;

        tx.commit()?;
        Ok(orphaned_hashes)
    }

    // --- mailboxes --------------------------------------------------------

    /// Insert or update a mailbox (by id).
    pub fn upsert_mailbox(&self, m: &Mailbox) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO mailboxes
                 (id, account_id, path, name, role, delimiter, subscribed, selectable, sync_by_default)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
                 path = excluded.path,
                 name = excluded.name,
                 role = excluded.role,
                 delimiter = excluded.delimiter,
                 subscribed = excluded.subscribed,
                 selectable = excluded.selectable,
                 sync_by_default = excluded.sync_by_default",
            params![
                m.id,
                m.account_id,
                m.path,
                m.name,
                m.role,
                m.delimiter,
                m.subscribed,
                m.selectable,
                m.sync_by_default
            ],
        )?;
        Ok(())
    }

    /// List mailboxes for an account, ordered by path.
    pub fn list_mailboxes(&self, account_id: &str) -> Result<Vec<Mailbox>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT id, account_id, path, name, role,
                    delimiter, subscribed, selectable, sync_by_default
             FROM mailboxes
             WHERE account_id = ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map(params![account_id], |r| {
            Ok(Mailbox {
                id: r.get(0)?,
                account_id: r.get(1)?,
                path: r.get(2)?,
                name: r.get(3)?,
                role: r.get(4)?,
                delimiter: r.get(5)?,
                subscribed: r.get(6)?,
                selectable: r.get(7)?,
                sync_by_default: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Total and unread message counts per mailbox for an account (sidebar badges).
    pub fn mailbox_counts(&self, account_id: &str) -> Result<Vec<MailboxCounts>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            // Selected FROM mailboxes so an emptied mailbox reports {0,0}
            // (the UI map only overwrites present keys), and snoozed mail is
            // left out because every list hides it.
            //
            // Snoozed mail is SUBTRACTED (a second aggregate over the small
            // snoozed set) instead of probed per message with NOT EXISTS: same
            // result, but the per-message probe cost ~30% of this query on the
            // 100k profile (#3687: 69 -> 47 ms per account, warm, sqlite3).
            "SELECT mb.id,
                    COALESCE(c.total, 0) - COALESCE(s.total, 0),
                    COALESCE(c.unread, 0) - COALESCE(s.unread, 0)
             FROM mailboxes mb
             LEFT JOIN (SELECT m.mailbox_id AS mailbox_id, COUNT(*) AS total,
                               SUM(CASE WHEN COALESCE(f.is_read, 0) = 0
                                        THEN 1 ELSE 0 END) AS unread
                        FROM messages m
                        LEFT JOIN message_flags f ON f.message_id = m.id
                        WHERE m.account_id = ?1
                        GROUP BY m.mailbox_id) c ON c.mailbox_id = mb.id
             LEFT JOIN (SELECT m.mailbox_id AS mailbox_id, COUNT(*) AS total,
                               SUM(CASE WHEN COALESCE(f.is_read, 0) = 0
                                        THEN 1 ELSE 0 END) AS unread
                        FROM snoozed_messages sn
                        JOIN messages m ON m.id = sn.message_id
                        LEFT JOIN message_flags f ON f.message_id = m.id
                        WHERE m.account_id = ?1
                        GROUP BY m.mailbox_id) s ON s.mailbox_id = mb.id
             WHERE mb.account_id = ?1",
        )?;
        let rows = stmt.query_map(params![account_id], |r| {
            Ok(MailboxCounts {
                mailbox_id: r.get(0)?,
                total: r.get(1)?,
                unread: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    // --- sync cursors -----------------------------------------------------

    /// Read the sync cursor for a mailbox, if any.
    pub fn get_sync_cursor(
        &self,
        account_id: &str,
        mailbox_id: &str,
    ) -> Result<Option<SyncCursor>, DbError> {
        let conn = self.lock_conn();
        let cursor = conn
            .query_row(
                "SELECT uid_validity, uid_next, highest_modseq, descriptor_checkpoint_uid
                 FROM sync_cursors
                 WHERE account_id = ?1 AND mailbox_id = ?2",
                params![account_id, mailbox_id],
                |r| {
                    Ok(SyncCursor {
                        uid_validity: r.get(0)?,
                        uid_next: r.get(1)?,
                        highest_modseq: r.get(2)?,
                        descriptor_checkpoint_uid: r.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(cursor)
    }

    /// Insert or update the sync cursor for a mailbox. `c.descriptor_checkpoint_uid`
    /// is written as given — callers that mean to keep an in-progress chunk
    /// checkpoint across an otherwise-unrelated cursor write must carry it
    /// forward themselves (see `set_descriptor_checkpoint` for the narrow,
    /// checkpoint-only write the chunked descriptor fetch actually uses).
    pub fn set_sync_cursor(
        &self,
        account_id: &str,
        mailbox_id: &str,
        c: &SyncCursor,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO sync_cursors
                 (account_id, mailbox_id, uid_validity, uid_next, highest_modseq,
                  descriptor_checkpoint_uid, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))
             ON CONFLICT(account_id, mailbox_id) DO UPDATE SET
                 uid_validity = excluded.uid_validity,
                 uid_next = excluded.uid_next,
                 highest_modseq = excluded.highest_modseq,
                 descriptor_checkpoint_uid = excluded.descriptor_checkpoint_uid,
                 updated_at = datetime('now')",
            params![
                account_id,
                mailbox_id,
                c.uid_validity,
                c.uid_next,
                c.highest_modseq,
                c.descriptor_checkpoint_uid,
            ],
        )?;
        Ok(())
    }

    /// Advance (or seed) the in-progress chunked-descriptor-import checkpoint
    /// for a mailbox, WITHOUT touching `uid_next`/`highest_modseq` — the
    /// narrow write the chunked import makes after every
    /// successfully committed chunk. `uid_validity` is
    /// ALWAYS written, on first insert AND on conflict (Opus review of PR
    /// #180): the first chunk to commit after a UID space change is exactly
    /// what must correct a stale row left over from before that change was
    /// recognised — leaving `uid_validity` alone on conflict (the original
    /// bug) meant a resumed import's own checkpoint write could disagree
    /// with the row's recorded UID space, so the NEXT pass's `same_uid_space`
    /// check would see a mismatch and purge everything again, forever. This
    /// row is always written in the CURRENT `uid_validity` this call was
    /// given, which is what makes it self-correcting.
    pub fn set_descriptor_checkpoint(
        &self,
        account_id: &str,
        mailbox_id: &str,
        uid_validity: i64,
        checkpoint_uid: i64,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO sync_cursors
                 (account_id, mailbox_id, uid_validity, descriptor_checkpoint_uid, updated_at)
             VALUES (?1, ?2, ?3, ?4, datetime('now'))
             ON CONFLICT(account_id, mailbox_id) DO UPDATE SET
                 uid_validity = excluded.uid_validity,
                 descriptor_checkpoint_uid = excluded.descriptor_checkpoint_uid,
                 updated_at = datetime('now')",
            params![account_id, mailbox_id, uid_validity, checkpoint_uid],
        )?;
        Ok(())
    }

    /// Add a UID to the mailbox's decode-skip RETRY SET (;
    /// migration 0040's comment): a chunked descriptor fetch isolated it as
    /// undecodable and it must not be lost just because the checkpoint
    /// moves past it. `reason` must be a `DescriptorSkipReason`'s
    /// `as_str()` — a fixed classification, never the underlying protocol
    /// error's text.
    ///
    /// A new row is first due for a retry one hour after `now_epoch`
    /// (`decode_skip_retry_delay_secs(0)`). Idempotent on `(account_id,
    /// mailbox_id, uid_validity, uid)`: a repeat skip refreshes `reason`/
    /// `skipped_at` in place and deliberately KEEPS `attempts`/
    /// `next_retry_at` — re-importing a range (e.g. after a cleared
    /// checkpoint) must not reset a long backoff to one hour. Only the
    /// sync engine's retry pass removes a row, once the UID decodes or no
    /// longer exists.
    pub fn record_message_decode_skip(
        &self,
        account_id: &str,
        mailbox_id: &str,
        uid: i64,
        uid_validity: i64,
        reason: &str,
        now_epoch: i64,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO message_decode_skips
                 (account_id, mailbox_id, uid, uid_validity, reason, skipped_at,
                  attempts, next_retry_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'), 0, ?6)
             ON CONFLICT(account_id, mailbox_id, uid_validity, uid) DO UPDATE SET
                 reason = excluded.reason,
                 skipped_at = excluded.skipped_at",
            params![
                account_id,
                mailbox_id,
                uid,
                uid_validity,
                reason,
                now_epoch.saturating_add(crate::decode_skip_retry_delay_secs(0)),
            ],
        )?;
        Ok(())
    }

    /// A retry of a UID in the retry set still could not decode it: count
    /// the failed attempt and push `next_retry_at` out by the NEXT step of
    /// the backoff (4h, 24h, then 7 days thereafter — see
    /// `decode_skip_retry_delay_secs`). No-op if the row is gone.
    pub fn record_message_decode_retry_failure(
        &self,
        account_id: &str,
        mailbox_id: &str,
        uid_validity: i64,
        uid: i64,
        now_epoch: i64,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        let attempts: Option<i64> = conn
            .query_row(
                "SELECT attempts FROM message_decode_skips
                 WHERE account_id = ?1 AND mailbox_id = ?2 AND uid_validity = ?3 AND uid = ?4",
                params![account_id, mailbox_id, uid_validity, uid],
                |r| r.get(0),
            )
            .optional()?;
        let Some(attempts) = attempts else {
            return Ok(());
        };
        let attempts = attempts.saturating_add(1);
        conn.execute(
            "UPDATE message_decode_skips SET attempts = ?5, next_retry_at = ?6
             WHERE account_id = ?1 AND mailbox_id = ?2 AND uid_validity = ?3 AND uid = ?4",
            params![
                account_id,
                mailbox_id,
                uid_validity,
                uid,
                attempts,
                now_epoch.saturating_add(crate::decode_skip_retry_delay_secs(attempts)),
            ],
        )?;
        Ok(())
    }

    /// `(attempts, next_retry_at)` for one retry-set row, if present.
    pub fn message_decode_skip_backoff(
        &self,
        account_id: &str,
        mailbox_id: &str,
        uid_validity: i64,
        uid: i64,
    ) -> Result<Option<(i64, i64)>, DbError> {
        let conn = self.lock_conn();
        Ok(conn
            .query_row(
                "SELECT attempts, next_retry_at FROM message_decode_skips
                 WHERE account_id = ?1 AND mailbox_id = ?2 AND uid_validity = ?3 AND uid = ?4",
                params![account_id, mailbox_id, uid_validity, uid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// Count of UIDs currently in the mailbox's decode-skip retry set.
    pub fn count_message_decode_skips(
        &self,
        account_id: &str,
        mailbox_id: &str,
    ) -> Result<i64, DbError> {
        let conn = self.lock_conn();
        let n = conn.query_row(
            "SELECT COUNT(*) FROM message_decode_skips
             WHERE account_id = ?1 AND mailbox_id = ?2",
            params![account_id, mailbox_id],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// Up to `limit` UIDs from the mailbox's decode-skip retry set, in the
    /// CURRENT `uid_validity`, whose backoff has expired (`next_retry_at <=
    /// now_epoch`) — the most overdue first, so a large set drains fairly
    /// instead of always retrying the lowest UIDs (review of PR #180,
    /// finding 3: the old `ORDER BY uid LIMIT 200`, every pass, no backoff).
    pub fn list_due_message_decode_skips(
        &self,
        account_id: &str,
        mailbox_id: &str,
        uid_validity: i64,
        now_epoch: i64,
        limit: i64,
    ) -> Result<Vec<i64>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT uid FROM message_decode_skips
             WHERE account_id = ?1 AND mailbox_id = ?2 AND uid_validity = ?3
               AND next_retry_at <= ?4
             ORDER BY next_retry_at, uid
             LIMIT ?5",
        )?;
        let rows = stmt.query_map(
            params![account_id, mailbox_id, uid_validity, now_epoch, limit],
            |r| r.get(0),
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Remove one UID from the retry set — the sync engine's job once a
    /// retry-set fetch decodes it cleanly (or the server no longer reports
    /// it at all, e.g. deleted): either way there is nothing left to retry.
    pub fn delete_message_decode_skip(
        &self,
        account_id: &str,
        mailbox_id: &str,
        uid_validity: i64,
        uid: i64,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "DELETE FROM message_decode_skips
             WHERE account_id = ?1 AND mailbox_id = ?2 AND uid_validity = ?3 AND uid = ?4",
            params![account_id, mailbox_id, uid_validity, uid],
        )?;
        Ok(())
    }

    /// Drop every retry-set row for a mailbox whose `uid_validity` no longer
    /// matches the server's current one — a UIDVALIDITY reset means those
    /// UIDs describe messages in a space that no longer exists; retrying
    /// them would at best waste a round trip and at worst (if UIDs get
    /// reused) retry the wrong message entirely. Returns the number dropped.
    pub fn delete_stale_message_decode_skips(
        &self,
        account_id: &str,
        mailbox_id: &str,
        current_uid_validity: i64,
    ) -> Result<usize, DbError> {
        let conn = self.lock_conn();
        let n = conn.execute(
            "DELETE FROM message_decode_skips
             WHERE account_id = ?1 AND mailbox_id = ?2 AND uid_validity != ?3",
            params![account_id, mailbox_id, current_uid_validity],
        )?;
        Ok(n)
    }

    /// Delete the sync cursor for the mailbox at `path`, if any, AND its
    /// local message rows, in the SAME transaction — forces the next sync to
    /// treat it as never-synced (full re-import from UIDNEXT 1, within the
    /// normal history window) instead of resuming from a stale high-water
    /// mark over rows that no longer mean what their UIDs say. Mirrors the
    /// sync engine's own UIDVALIDITY-reset handling (`delete_messages_for_mailbox`
    /// alongside the cursor rewrite in `sync.rs`) — a cleared cursor with
    /// stale rows left behind is exactly that same inconsistency: if the
    /// server's UIDVALIDITY changes again before the next sync, those rows
    /// would look current under the new cursor, and a later action keyed on
    /// their UIDs (e.g. Empty Trash) could act on the wrong UID space.
    ///
    /// Used only to recover from a legacy `empty_trash` payload (see
    /// `castellum_core::replay`): an old build already deleted MOST of the
    /// local Trash rows without a UID snapshot when it originally queued the
    /// op, but anything that landed in Trash since (another client, a sync
    /// that ran before replay) is still sitting locally with no reliable
    /// snapshot behind it either. No evaluation-mode gate — this only clears
    /// local bookkeeping, exactly like `set_sync_cursor`.
    pub fn clear_sync_cursor(&self, account_id: &str, path: &str) -> Result<(), DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let mailbox_id: Option<String> = tx
            .query_row(
                "SELECT id FROM mailboxes WHERE account_id = ?1 AND path = ?2",
                params![account_id, path],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(mailbox_id) = mailbox_id {
            tx.execute(
                "DELETE FROM sync_cursors WHERE account_id = ?1 AND mailbox_id = ?2",
                params![account_id, mailbox_id],
            )?;
            // Legacy empty_trash recovery: purges Trash rows with no remote
            // change for sync.rs to observe. A bulk mailbox purge: sweep the
            // threads it emptied and force the full-rebuild fallback in the
            // same transaction (; PR #181 review, finding 1).
            purge_mailbox_messages(&tx, account_id, &mailbox_id)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// [`Self::set_sync_cursor`] for the sync engine, compare-and-set on the
    /// modseq: UIDVALIDITY/UIDNEXT are always written, but `highest_modseq`
    /// only if the stored value is still `expected_modseq` — the value this
    /// sync read (or last wrote) for the mailbox. Anything else means a
    /// local-only flag change invalidated the baseline mid-sync (see
    /// `invalidate_mailbox_modseq`); that invalidation must win, or the
    /// divergence it flags would never be converged. A missing row is simply
    /// inserted. Returns whether `c.highest_modseq` was stored.
    pub fn set_sync_cursor_if_modseq_is(
        &self,
        account_id: &str,
        mailbox_id: &str,
        c: &SyncCursor,
        expected_modseq: Option<i64>,
    ) -> Result<bool, DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let stored: Option<Option<i64>> = tx
            .query_row(
                "SELECT highest_modseq FROM sync_cursors
                 WHERE account_id = ?1 AND mailbox_id = ?2",
                params![account_id, mailbox_id],
                |r| r.get(0),
            )
            .optional()?;
        let modseq_taken = stored.is_none_or(|current| current == expected_modseq);
        // `descriptor_checkpoint_uid` is always written as `c` carries it —
        // this is the same call `persist_cursor` uses once a mailbox's
        // descriptor step for the pass has finished, and it always passes
        // `None` there to clear any in-progress chunk checkpoint (see the
        // migration 0040 comment). No compare-and-set needed: unlike
        // `highest_modseq`, nothing else writes this column concurrently.
        tx.execute(
            "INSERT INTO sync_cursors
                 (account_id, mailbox_id, uid_validity, uid_next, highest_modseq,
                  descriptor_checkpoint_uid, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))
             ON CONFLICT(account_id, mailbox_id) DO UPDATE SET
                 uid_validity = excluded.uid_validity,
                 uid_next = excluded.uid_next,
                 highest_modseq = CASE WHEN ?7 THEN excluded.highest_modseq
                                       ELSE sync_cursors.highest_modseq END,
                 descriptor_checkpoint_uid = excluded.descriptor_checkpoint_uid,
                 updated_at = datetime('now')",
            params![
                account_id,
                mailbox_id,
                c.uid_validity,
                c.uid_next,
                c.highest_modseq,
                c.descriptor_checkpoint_uid,
                modseq_taken
            ],
        )?;
        tx.commit()?;
        Ok(modseq_taken)
    }

    // --- messages ---------------------------------------------------------

    /// Insert or update a message with its flags, addresses and (optional) body,
    /// atomically. Idempotent on re-sync (keyed by message id). One
    /// transaction per call; see [`Self::upsert_messages_batch`] for a sync
    /// pass importing many messages at once.
    pub fn upsert_message(&self, m: &NewMessage) -> Result<(), DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let mut stashes = None;
        upsert_message_tx(&tx, m, &mut stashes)?;
        tx.commit()?;
        Ok(())
    }

    /// [`Self::upsert_message`] for a whole chunk at once, in ONE
    /// transaction: a first sync's descriptor import was
    /// paying its own transaction's commit cost per message (~0.05ms on this
    /// Mac WAL/`synchronous=FULL`, vs ~0.001ms/row inside one), and every
    /// statement was re-prepared instead of reused. The caller chunks into
    /// `castellum_core::sync::DESCRIPTOR_UPSERT_BATCH_SIZE` (200) so this
    /// call, and so the DB mutex hold it needs, stays bounded -- measured
    /// p95 ~6-8ms per 200-message batch (mock-provider `sync_perf_gate`,
    /// 100k-message corpus; the pre-#2503 baseline's 18-35ms/100-message
    /// figure in project-docs/perf/2026-09-sync-baseline.md measured many
    /// separate single-message transactions, not what one batched
    /// transaction actually costs, so it is NOT the right number to scale
    /// from here) -- and advances sync progress once per chunk rather than
    /// per message.
    ///
    /// All-or-nothing: an error partway rolls back the WHOLE chunk (the
    /// `Transaction` is dropped without a commit), matching the cursor's own
    /// all-or-nothing semantics -- `sync.rs` only calls `persist_cursor`
    /// after a whole mailbox's chunks succeed, never mid-chunk, so a chunk
    /// that partially landed would leave rows the next sync's cursor-based
    /// resume has no way to re-derive.
    pub fn upsert_messages_batch(&self, messages: &[NewMessage]) -> Result<(), DbError> {
        if messages.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        // Outstanding Mac-only tag stashes are loaded ONCE per transaction,
        // not probed per message: a per-message probe walks the whole
        // (never pruned) operations table.
        let mut stashes = None;
        for m in messages {
            upsert_message_tx(&tx, m, &mut stashes)?;
        }
        tx.commit()?;
        Ok(())
    }
}

/// The body of [`Database::upsert_message`], factored out so
/// [`Database::upsert_messages_batch`] can run it for many messages inside
/// ONE caller-owned transaction instead of one per message. Every statement
/// is `prepare_cached` (keyed by the exact literal SQL text, which is what
/// every call site below passes) so a multi-message batch reuses the
/// prepared plan instead of re-parsing/re-planning it per row.
fn upsert_message_tx(
    tx: &rusqlite::Transaction<'_>,
    m: &NewMessage,
    stashes: &mut Option<Vec<MoveStash>>,
) -> Result<(), DbError> {
    let has_attachments = m
        .attachments
        .as_ref()
        .map_or(m.has_attachments, |attachments| {
            attachments.iter().any(|a| a.is_real_attachment())
        });
    let attachments_are_authoritative = m.attachments.is_some();
    // Also the pre-upsert (rfc822_message_id, sent_at, subject) snapshot
    // for the/#2504 dirty check below -- one query serves all
    // three purposes, no extra round trip added to this hot path.
    let existing: Option<(Option<String>, Option<i64>, Option<String>)> = tx
        .prepare_cached("SELECT rfc822_message_id, sent_at, subject FROM messages WHERE id = ?1")?
        .query_row(params![m.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    let message_existed = existing.is_some();
    if !message_existed {
        // A brand-new message always needs a thread assignment -- unlike
        // the two checks below (which only fire when an EXISTING row's
        // thread-relevant fields actually changed), there is no "unchanged"
        // case for a row that didn't exist a moment ago.
        mark_threads_dirty(tx, &m.account_id, &[m.id.as_str()])?;
    }
    let insert_is_new = !message_existed;
    tx.prepare_cached(
        "INSERT INTO messages
             (id, account_id, mailbox_id, provider_uid, rfc822_message_id, subject,
              from_name, from_address, sent_at, snippet, size_bytes, has_attachments)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT(id) DO UPDATE SET
             provider_uid = excluded.provider_uid,
             rfc822_message_id = COALESCE(excluded.rfc822_message_id, messages.rfc822_message_id),
             subject = COALESCE(excluded.subject, messages.subject),
             from_name = COALESCE(excluded.from_name, messages.from_name),
             from_address = COALESCE(excluded.from_address, messages.from_address),
             sent_at = COALESCE(excluded.sent_at, messages.sent_at),
             snippet = COALESCE(excluded.snippet, messages.snippet),
             size_bytes = COALESCE(excluded.size_bytes, messages.size_bytes),
             has_attachments = CASE
                 WHEN ?13 THEN excluded.has_attachments
                 WHEN excluded.has_attachments THEN 1
                 ELSE messages.has_attachments
             END",
    )?
    .execute(params![
        m.id,
        m.account_id,
        m.mailbox_id,
        m.provider_uid,
        m.rfc822_message_id,
        m.subject,
        m.from_name,
        m.from_address,
        m.sent_at,
        m.snippet,
        m.size_bytes,
        has_attachments,
        attachments_are_authoritative,
    ])?;

    // Loaded lazily, at most once per transaction and only when a row is
    // really new: the load scans the (never pruned) operations table, and an
    // existing-row upsert is the hot body-persist path.
    if insert_is_new {
        if stashes.is_none() {
            *stashes = Some(load_move_stashes(tx)?);
        }
        if let Some(stashes) = stashes.as_mut().filter(|s| !s.is_empty()) {
            reattach_local_keywords_after_move(tx, m, stashes)?;
        }
    }

    // Provider import owns flags only when the row is first created.
    // Existing flags may contain unreplayed local intent; only explicit
    // reconciliation is allowed to replace them later.
    tx.prepare_cached(
        "INSERT OR IGNORE INTO message_flags (message_id, is_read, is_starred)
         VALUES (?1, ?2, ?3)",
    )?
    .execute(params![m.id, m.is_read, m.is_starred])?;

    // A descriptor retry must not erase richer addresses/references that a
    // previous full-body parse discovered. New descriptors seed these rows;
    // an authoritative hydrated body may replace them later.
    if !message_existed || attachments_are_authoritative {
        // Snapshot the reference set an EXISTING row already had, so a
        // hydration pass (fetched == 0, no remote change for sync to see)
        // can tell whether it actually changed thread membership. A brand
        // new row (`!message_existed`) needs no snapshot: it arrives
        // within a sync pass that already reports `fetched > 0`, which
        // already forces `rebuild_threads` (see sync.rs's
        // `any_remote_change`).
        let old_refs: Vec<String> = if message_existed && attachments_are_authoritative {
            tx.prepare_cached(
                "SELECT ref_message_id FROM message_references
                 WHERE message_id = ?1 ORDER BY ordinal",
            )?
            .query_map(params![m.id], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };

        tx.prepare_cached("DELETE FROM message_addresses WHERE message_id = ?1")?
            .execute(params![m.id])?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO message_addresses (message_id, kind, name, address, ordinal)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for (ordinal, a) in m.addresses.iter().enumerate() {
                stmt.execute(params![m.id, a.kind, a.name, a.address, ordinal as i64])?;
            }
        }

        tx.prepare_cached("DELETE FROM message_references WHERE message_id = ?1")?
            .execute(params![m.id])?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO message_references (message_id, ref_message_id, ordinal)
                 VALUES (?1, ?2, ?3)",
            )?;
            for (ordinal, r) in m.references.iter().enumerate() {
                stmt.execute(params![m.id, r, ordinal as i64])?;
            }
        }

        // Body hydration on an existing descriptor-only row that changes
        // the References chain changes thread grouping (`rebuild_threads`
        // derives membership from `message_references`), but sync-time
        // hydration passes fetch 0 new messages, reconcile 0 flags and
        // vanish 0 UIDs, so `any_remote_change` in sync.rs never sees it
        // — and standalone hydrators (`hydrate_pending_bodies_json`,
        // `hydrate_mailbox_batch`, open-time
        // `hydrate_message_body_with_provider`) never call sync.rs at
        // all. Mark the account dirty here, in the DB layer, so no
        // caller can forget — but only when the reference set actually
        // differs, so an ordinary re-hydration with an unchanged
        // References header costs nothing extra.
        if message_existed && attachments_are_authoritative && old_refs != m.references {
            mark_threads_dirty(tx, &m.account_id, &[m.id.as_str()])?;
        }
    }

    if attachments_are_authoritative
        || m.text_body.is_some()
        || m.sanitized_html.is_some()
        || m.html_body.is_some()
    {
        tx.prepare_cached(
            "INSERT INTO message_bodies (message_id, text_body, sanitized_html, html_body, has_remote_content, body_fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))
             ON CONFLICT(message_id) DO UPDATE SET
                 text_body = excluded.text_body,
                 sanitized_html = excluded.sanitized_html,
                 html_body = excluded.html_body,
                 has_remote_content = excluded.has_remote_content,
                 body_fetched_at = datetime('now')",
        )?
        .execute(params![
            m.id,
            m.text_body,
            m.sanitized_html,
            m.html_body,
            m.has_remote_content
        ])?;
    }

    // A hydrated full-body parse supplies an authoritative attachment list.
    // Descriptor-only sync uses `None`, which must not erase rows persisted
    // by an earlier hydration.
    if let Some(attachments) = &m.attachments {
        tx.prepare_cached("DELETE FROM message_attachments WHERE message_id = ?1")?
            .execute(params![m.id])?;
        let mut stmt = tx.prepare_cached(
            "INSERT INTO message_attachments
                 (message_id, ordinal, filename, content_type, size_bytes,
                  section, content_id, is_inline, transfer_encoding)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        for attachment in attachments {
            stmt.execute(params![
                m.id,
                attachment.ordinal,
                attachment.filename,
                attachment.content_type,
                attachment.size_bytes,
                attachment.section,
                attachment.content_id,
                attachment.is_inline,
                attachment.transfer_encoding,
            ])?;
        }
    }

    let (indexed_subject, indexed_sender, new_rfc822_message_id, new_sent_at) = tx
        .prepare_cached(
            "SELECT COALESCE(subject, ''),
                    TRIM(COALESCE(from_name, '') || ' ' || COALESCE(from_address, '')),
                    rfc822_message_id, sent_at
             FROM messages WHERE id = ?1",
        )?
        .query_row(params![m.id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        })?;
    //: hydration can fill in a previously-null
    // rfc822_message_id (the envelope had no Message-ID; the full body
    // parse discovers one), or change sent_at (INTERNALDATE -> the Date
    // header) -- either regroups or reorders a thread exactly like the
    // References-chain change above, with the same blind spot: sync.rs
    // never sees it as a remote change. Also: a subject
    // change on an EXISTING row, with neither a ref/rfc822/sent_at change
    // to otherwise catch it -- `threads.subject` is copied from whichever
    // message is the thread's root, so a root's later-corrected subject
    // (e.g. a blank descriptor subject filled in by hydration) must still
    // reach the projection, or an incremental rebuild would never revisit
    // that thread and it would go stale forever, not just until the next
    // sync (found by this file's `incremental_rebuild_matches_full_
    // rebuild_after_random_mutations` property test). Mark dirty here too,
    // only when something actually moved, so an ordinary re-hydration that
    // changes none of the three costs nothing extra.
    if let Some((old_rfc822_message_id, old_sent_at, old_subject)) = &existing {
        // Filled in OR changed to a different id (PR #181
        // follow-up): either way replies citing the old/new id regroup.
        let message_id_changed = *old_rfc822_message_id != new_rfc822_message_id;
        let sent_at_changed = *old_sent_at != new_sent_at;
        let subject_changed = old_subject.clone().unwrap_or_default() != indexed_subject;
        if message_id_changed || sent_at_changed || subject_changed {
            mark_threads_dirty(tx, &m.account_id, &[m.id.as_str()])?;
        }
    }
    // `message_fts.message_id` is UNINDEXED, so filtering on it used to
    // be a full scan of the FTS table — O(rows) per call, which made a
    // first sync O(n^2) (project-docs/perf/2026-09-sync-baseline.md) and
    // an 8ms/message hydration re-index at 100k. `message_fts_rowid`
    // (migration 0038) maps this message id to a stable integer, PK-
    // looked-up here, so every message_fts write below goes `WHERE rowid
    // = ?` instead — the table's own indexed key.
    let fts_rowid = ensure_fts_rowid(tx, &m.id)?;
    if !message_existed || attachments_are_authoritative {
        // A brand-new message cannot have an FTS row: this is the only
        // writer, and the `messages_before_delete_fts` trigger removes the
        // row whenever its message is deleted. So only delete-before-
        // insert when the message already existed (bounded hydration
        // re-index); either way the delete is a cheap rowid lookup, not
        // a scan.
        if message_existed {
            tx.prepare_cached("DELETE FROM message_fts WHERE rowid = ?1")?
                .execute(params![fts_rowid])?;
        }
        tx.prepare_cached(
            "INSERT INTO message_fts (rowid, message_id, subject, sender, body)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?
        .execute(params![
            fts_rowid,
            m.id,
            indexed_subject,
            indexed_sender,
            m.text_body.as_deref().unwrap_or(""),
        ])?;
    } else {
        let updated = tx
            .prepare_cached("UPDATE message_fts SET subject = ?2, sender = ?3 WHERE rowid = ?1")?
            .execute(params![fts_rowid, indexed_subject, indexed_sender])?;
        if updated == 0 {
            tx.prepare_cached(
                "INSERT INTO message_fts (rowid, message_id, subject, sender, body)
                 VALUES (?1, ?2, ?3, ?4, '')",
            )?
            .execute(params![fts_rowid, m.id, indexed_subject, indexed_sender])?;
        }
    }

    Ok(())
}

impl Database {
    /// Store SPF/DKIM/DMARC trust signals for `message_id` (S48). Called
    /// once per hydrated message, right after `upsert_message`, from
    /// `new_message_from_raw`'s callers — kept OUT of `upsert_message` itself
    /// so `NewMessage` (constructed at ~25 call sites across the crate) never
    /// needed a new required field. A message row must already exist;
    /// `ON DELETE CASCADE` on `messages(id)` removes this row with it.
    pub fn upsert_message_trust(
        &self,
        message_id: &str,
        trust: &TrustSignals,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO message_trust (message_id, spf, dkim, dmarc, authserv_id, computed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))
             ON CONFLICT(message_id) DO UPDATE SET
                 spf = excluded.spf,
                 dkim = excluded.dkim,
                 dmarc = excluded.dmarc,
                 authserv_id = excluded.authserv_id,
                 computed_at = datetime('now')",
            params![
                message_id,
                trust_result_str(trust.spf),
                trust_result_str(trust.dkim),
                trust_result_str(trust.dmarc),
                trust.authserv_id,
            ],
        )?;
        Ok(())
    }

    // --- calendar invites (S55) ---------------------------------------------

    /// Store (or update) a message's parsed calendar invite — everything
    /// EXCEPT `our_response`/`responded_at`, which this call deliberately
    /// leaves untouched (the `ON CONFLICT` clause never assigns them) so a
    /// re-hydration can never wipe a response the user already recorded.
    /// Called once per hydrated message, right after `upsert_message`,
    /// alongside `upsert_message_trust` — see `new_message_from_raw`'s
    /// callers in `castellum_core::sync`. A message row must already exist;
    /// `ON DELETE CASCADE` on `messages(id)` removes this row with it.
    pub fn upsert_calendar_invite(
        &self,
        message_id: &str,
        invite: &CalendarInvite,
    ) -> Result<(), DbError> {
        let attendees_json = serde_json::to_string(&invite.attendees)
            .map_err(|e| DbError(format!("serialize attendees: {e}")))?;
        let (dtstart_raw, dtstart_tzid, dtstart_is_utc, dtstart_is_date_only) =
            datetime_columns(invite.dtstart.as_ref());
        let (dtend_raw, dtend_tzid, dtend_is_utc, dtend_is_date_only) =
            datetime_columns(invite.dtend.as_ref());
        let (rid_raw, rid_tzid, rid_is_utc, rid_is_date_only) =
            datetime_columns(invite.recurrence_id.as_ref());
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO calendar_invites
                 (message_id, uid, sequence, method, summary,
                  dtstart_raw, dtstart_tzid, dtstart_is_utc, dtstart_is_date_only,
                  dtend_raw, dtend_tzid, dtend_is_utc, dtend_is_date_only,
                  organizer_email, organizer_name, attendees_json, location, description,
                  recurrence_id_raw, recurrence_id_tzid, recurrence_id_is_utc, recurrence_id_is_date_only,
                  updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
                     ?19, ?20, ?21, ?22, datetime('now'))
             ON CONFLICT(message_id) DO UPDATE SET
                 uid = excluded.uid,
                 sequence = excluded.sequence,
                 method = excluded.method,
                 summary = excluded.summary,
                 dtstart_raw = excluded.dtstart_raw,
                 dtstart_tzid = excluded.dtstart_tzid,
                 dtstart_is_utc = excluded.dtstart_is_utc,
                 dtstart_is_date_only = excluded.dtstart_is_date_only,
                 dtend_raw = excluded.dtend_raw,
                 dtend_tzid = excluded.dtend_tzid,
                 dtend_is_utc = excluded.dtend_is_utc,
                 dtend_is_date_only = excluded.dtend_is_date_only,
                 organizer_email = excluded.organizer_email,
                 organizer_name = excluded.organizer_name,
                 attendees_json = excluded.attendees_json,
                 location = excluded.location,
                 description = excluded.description,
                 recurrence_id_raw = excluded.recurrence_id_raw,
                 recurrence_id_tzid = excluded.recurrence_id_tzid,
                 recurrence_id_is_utc = excluded.recurrence_id_is_utc,
                 recurrence_id_is_date_only = excluded.recurrence_id_is_date_only,
                 updated_at = datetime('now')",
            params![
                message_id,
                invite.uid,
                invite.sequence,
                invite.method,
                invite.summary,
                dtstart_raw,
                dtstart_tzid,
                dtstart_is_utc,
                dtstart_is_date_only,
                dtend_raw,
                dtend_tzid,
                dtend_is_utc,
                dtend_is_date_only,
                invite.organizer_email,
                invite.organizer_name,
                attendees_json,
                invite.location,
                invite.description,
                rid_raw,
                rid_tzid,
                rid_is_utc,
                rid_is_date_only,
            ],
        )?;
        Ok(())
    }

    /// The calendar invite for `message_id`, if any, with `our_response`/
    /// `responded_at` filled in from the durable columns. `None` when the
    /// message has no `calendar_invites` row (no `text/calendar` part was
    /// found at hydration, or the message hasn't been hydrated yet).
    pub fn get_calendar_invite(&self, message_id: &str) -> Result<Option<CalendarInvite>, DbError> {
        let conn = self.lock_conn();
        #[allow(clippy::type_complexity)]
        let row: Option<(
            String,
            i64,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            i64,
            i64,
            Option<String>,
            Option<String>,
            i64,
            i64,
            Option<String>,
            Option<String>,
            String,
            Option<String>,
            Option<i64>,
            Option<String>,
            Option<String>,
        )> = conn
            .query_row(
                "SELECT uid, sequence, method, summary,
                        dtstart_raw, dtstart_tzid, dtstart_is_utc, dtstart_is_date_only,
                        dtend_raw, dtend_tzid, dtend_is_utc, dtend_is_date_only,
                        organizer_email, organizer_name, attendees_json,
                        our_response, responded_at, location, description
                 FROM calendar_invites WHERE message_id = ?1",
                params![message_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                        r.get(10)?,
                        r.get(11)?,
                        r.get(12)?,
                        r.get(13)?,
                        r.get(14)?,
                        r.get(15)?,
                        r.get(16)?,
                        r.get(17)?,
                        r.get(18)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            uid,
            sequence,
            method,
            summary,
            dtstart_raw,
            dtstart_tzid,
            dtstart_is_utc,
            dtstart_is_date_only,
            dtend_raw,
            dtend_tzid,
            dtend_is_utc,
            dtend_is_date_only,
            organizer_email,
            organizer_name,
            attendees_json,
            our_response,
            responded_at,
            location,
            description,
        )) = row
        else {
            return Ok(None);
        };
        let attendees: Vec<CalendarInviteAttendee> = serde_json::from_str(&attendees_json)
            .map_err(|e| DbError(format!("deserialize attendees: {e}")))?;
        let recurrence_id: Option<CalendarInviteDateTime> = conn
            .query_row(
                "SELECT recurrence_id_raw, recurrence_id_tzid, recurrence_id_is_utc,
                        recurrence_id_is_date_only
                 FROM calendar_invites WHERE message_id = ?1",
                params![message_id],
                |r| {
                    Ok(r.get::<_, Option<String>>(0)?
                        .map(|raw| CalendarInviteDateTime {
                            raw,
                            tzid: r.get(1).ok().flatten(),
                            is_utc: r.get::<_, i64>(2).unwrap_or(0) != 0,
                            is_date_only: r.get::<_, i64>(3).unwrap_or(0) != 0,
                        }))
                },
            )
            .optional()?
            .flatten();
        // Superseded by a later invite for the SAME meeting (and the same occurrence) in this
        // account: a higher SEQUENCE, or the same SEQUENCE when the later one is a CANCEL.
        // Derived on read so it also follows mail that arrives after this message.
        // A later invite (higher SEQUENCE, or a CANCEL at the same SEQUENCE) for the same meeting
        // and occurrence needs an organizer on both sides, equal case-insensitively. It only
        // AUTO-supersedes when the later message is also authenticated as the organizer: its From
        // is exactly the organizer AND DMARC passed. Anything else (anyone who knows a UID can
        // mail a CANCEL) is advisory only: `possibly_superseded`. Domain alignment alone is NOT
        // enough (any user at the organizer's domain would qualify). Not available yet: an
        // explicit ORGANIZER SENT-BY delegate path (not parsed/stored), so a delegate's update is
        // advisory; DKIM alignment likewise (message_trust keeps no signing domain).
        let find_later = |verified: bool| -> Result<Option<String>, DbError> {
            if method == "CANCEL" || method == "REPLY" || organizer_email.is_none() {
                return Ok(None);
            }
            let auth = if verified {
                "AND lower(m.from_address) = lower(ci.organizer_email)
                 AND EXISTS (SELECT 1 FROM message_trust t
                              WHERE t.message_id = ci.message_id AND t.dmarc = 'pass')"
            } else {
                ""
            };
            let sql = format!(
                "SELECT ci.method FROM calendar_invites ci
                   JOIN messages m ON m.id = ci.message_id
                   JOIN messages me ON me.id = ?1
                  WHERE ci.message_id <> ?1 AND m.account_id = me.account_id
                    AND ci.uid = ?2
                    AND ci.organizer_email IS NOT NULL
                    AND lower(ci.organizer_email) = lower(?5)
                    {auth}
                    AND ci.method IN ('REQUEST', 'CANCEL', 'PUBLISH')
                    AND COALESCE(ci.recurrence_id_raw, '') = COALESCE(?3, '')
                    AND (ci.sequence > ?4 OR (ci.sequence = ?4 AND ci.method = 'CANCEL'))
                  ORDER BY (ci.method = 'CANCEL') DESC, ci.sequence DESC LIMIT 1"
            );
            Ok(conn
                .query_row(
                    &sql,
                    params![
                        message_id,
                        uid,
                        recurrence_id.as_ref().map(|d| d.raw.as_str()),
                        sequence,
                        organizer_email
                    ],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
                .map(|later| {
                    if later == "CANCEL" {
                        "cancelled".to_owned()
                    } else {
                        "updated".to_owned()
                    }
                }))
        };
        let superseded = find_later(true)?;
        let possibly_superseded = if superseded.is_some() {
            None
        } else {
            find_later(false)?
        };
        Ok(Some(CalendarInvite {
            uid,
            sequence,
            method,
            summary,
            dtstart: dtstart_raw.map(|raw| CalendarInviteDateTime {
                raw,
                tzid: dtstart_tzid,
                is_utc: dtstart_is_utc != 0,
                is_date_only: dtstart_is_date_only != 0,
            }),
            dtend: dtend_raw.map(|raw| CalendarInviteDateTime {
                raw,
                tzid: dtend_tzid,
                is_utc: dtend_is_utc != 0,
                is_date_only: dtend_is_date_only != 0,
            }),
            organizer_email,
            organizer_name,
            attendees,
            location,
            description,
            our_response,
            responded_at,
            recurrence_id,
            superseded,
            possibly_superseded,
        }))
    }

    /// Record the user's own Accept/Decline/Tentative response. `response`
    /// is stored verbatim (the caller validates it — see
    /// `castellum_core::calendar_invites::respond_to_invite`). Returns
    /// `false` (no row updated, no error) if the message has no calendar
    /// invite row; the caller treats that as a bug, since the Accept/
    /// Decline UI only ever exists on a message that has one.
    pub fn record_calendar_invite_response(
        &self,
        message_id: &str,
        response: &str,
        responded_at: i64,
    ) -> Result<bool, DbError> {
        let conn = self.lock_conn();
        let updated = conn.execute(
            "UPDATE calendar_invites SET our_response = ?2, responded_at = ?3
             WHERE message_id = ?1",
            params![message_id, response, responded_at],
        )?;
        Ok(updated > 0)
    }

    /// Put a calendar invite's stored response back to a previous value
    /// (`None` clears it). Used to roll back an optimistic record when the
    /// reply could not be queued.
    pub fn restore_calendar_invite_response(
        &self,
        message_id: &str,
        response: Option<&str>,
        responded_at: Option<i64>,
    ) -> Result<bool, DbError> {
        let conn = self.lock_conn();
        let updated = conn.execute(
            "UPDATE calendar_invites SET our_response = ?2, responded_at = ?3
             WHERE message_id = ?1",
            params![message_id, response, responded_at],
        )?;
        Ok(updated > 0)
    }

    /// Remember (or update) whether `from_address` may load remote content
    /// automatically for `account_id`, without a per-message prompt.
    /// `from_address` is lower-cased before storage.
    pub fn set_sender_remote_content_allowed(
        &self,
        account_id: &str,
        from_address: &str,
        allowed: bool,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO sender_remote_content_policy (account_id, from_address, allowed, updated_at)
             VALUES (?1, ?2, ?3, datetime('now'))
             ON CONFLICT(account_id, from_address) DO UPDATE SET
                 allowed = excluded.allowed,
                 updated_at = datetime('now')",
            params![account_id, from_address.trim().to_ascii_lowercase(), allowed],
        )?;
        Ok(())
    }

    /// `None` = no remembered policy for this sender (the caller falls back
    /// to the per-message prompt / global preference); `Some(bool)` = an
    /// explicit remembered decision.
    pub fn sender_remote_content_allowed(
        &self,
        account_id: &str,
        from_address: &str,
    ) -> Result<Option<bool>, DbError> {
        let conn = self.lock_conn();
        conn.query_row(
            "SELECT allowed FROM sender_remote_content_policy WHERE account_id = ?1 AND from_address = ?2",
            params![account_id, from_address.trim().to_ascii_lowercase()],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .map(|v| v.map(|n| n != 0))
        .map_err(DbError::from)
    }

    /// Every remembered sender policy across every account, newest first —
    /// the Settings > General "Remote content senders" review list.
    pub fn list_sender_remote_content_policies(
        &self,
    ) -> Result<Vec<SenderRemoteContentPolicy>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT account_id, from_address, allowed, updated_at
             FROM sender_remote_content_policy ORDER BY updated_at DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(SenderRemoteContentPolicy {
                    account_id: r.get(0)?,
                    from_address: r.get(1)?,
                    allowed: r.get::<_, i64>(2)? != 0,
                    updated_at: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Forget a remembered sender policy (Settings' Revoke action). A no-op
    /// (not an error) if none exists.
    pub fn revoke_sender_remote_content(
        &self,
        account_id: &str,
        from_address: &str,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "DELETE FROM sender_remote_content_policy WHERE account_id = ?1 AND from_address = ?2",
            params![account_id, from_address.trim().to_ascii_lowercase()],
        )?;
        Ok(())
    }

    /// List persisted incoming attachment metadata in MIME attachment order.
    pub fn list_message_attachments(
        &self,
        message_id: &str,
    ) -> Result<Vec<AttachmentDescriptor>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT ordinal, filename, content_type, size_bytes, section, content_id, is_inline,
                    transfer_encoding
             FROM message_attachments
             WHERE message_id = ?1
             ORDER BY ordinal",
        )?;
        let rows = stmt.query_map(params![message_id], |r| {
            Ok(AttachmentDescriptor {
                ordinal: r.get(0)?,
                filename: r.get(1)?,
                content_type: r.get(2)?,
                size_bytes: r.get(3)?,
                section: r.get(4)?,
                content_id: r.get(5)?,
                is_inline: r.get(6)?,
                transfer_encoding: r.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(DbError::from)
    }

    /// Delete all messages (and, via cascade, their addresses/flags/bodies) in a
    /// mailbox. Used to purge stale rows when a provider's UID space resets.
    pub fn delete_messages_for_mailbox(
        &self,
        account_id: &str,
        mailbox_id: &str,
    ) -> Result<(), DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        // A UIDVALIDITY reset (or an interrupted first import, the tracker
        // #2619) purges every local row for the mailbox — removing thread
        // members without any remote change for sync.rs's
        // `any_remote_change` to see (this purge IS the reconciliation; it
        // reports nothing fetched/reconciled/vanished). Mark the account
        // dirty in the same transaction so the next sync's rebuild-skip
        // decision sees it, but only when something was actually deleted.
        // A UIDVALIDITY reset (or an interrupted first import) is exactly
        // the case the incremental engine is not meant to chase -- force
        // the full-rebuild fallback, and sweep the threads
        // it emptied right away (PR #181 review, finding 1).
        purge_mailbox_messages(&tx, account_id, mailbox_id)?;
        tx.commit()?;
        Ok(())
    }

    /// Snapshot the (provider_uid, is_read, is_starred) of every stored message
    /// in a mailbox that has a provider UID. Used by the sync engine to diff
    /// local flag state against a remote `UID FETCH FLAGS` descriptor snapshot.
    /// A message with no `message_flags` row defaults to unread/unstarred.
    pub fn list_local_uid_flags(
        &self,
        account_id: &str,
        mailbox_id: &str,
    ) -> Result<Vec<(i64, bool, bool)>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT m.provider_uid,
                    COALESCE(f.is_read, 0),
                    COALESCE(f.is_starred, 0)
             FROM messages m
             LEFT JOIN message_flags f ON f.message_id = m.id
             WHERE m.account_id = ?1 AND m.mailbox_id = ?2 AND m.provider_uid IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![account_id, mailbox_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, bool>(1)?,
                r.get::<_, bool>(2)?,
            ))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// List provider UIDs for stored messages that do not yet have a body row.
    /// Used by descriptor-first sync to hydrate a small newest-first batch after
    /// envelope import without scanning the whole mailbox.
    pub fn list_body_missing_uids(
        &self,
        account_id: &str,
        mailbox_id: &str,
        limit: i64,
    ) -> Result<Vec<i64>, DbError> {
        if limit <= 0 {
            return Ok(vec![]);
        }
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT m.provider_uid
             FROM messages m
             LEFT JOIN message_bodies b ON b.message_id = m.id
             WHERE m.account_id = ?1
               AND m.mailbox_id = ?2
               AND m.provider_uid IS NOT NULL
               AND b.message_id IS NULL
             ORDER BY COALESCE(m.sent_at, 0) DESC, m.provider_uid DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![account_id, mailbox_id, limit], |r| r.get(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Newest-first `(provider_uid, known_size)` pairs for messages missing
    /// bodies whose stored RFC822 size, when known, does not exceed
    /// `max_message_bytes`. Same missing-body predicate as
    /// [`Self::list_body_missing_uids`]. Oversized rows are excluded HERE so
    /// they cannot starve smaller bodies behind them in the newest-first
    /// order; they stay in the durable queue for explicit body-on-open
    /// hydration. Size-unknown rows are included — the caller budgets them
    /// with a conservative estimate.
    pub fn list_body_missing_uids_with_sizes(
        &self,
        account_id: &str,
        mailbox_id: &str,
        limit: i64,
        max_message_bytes: i64,
    ) -> Result<Vec<(i64, Option<i64>)>, DbError> {
        if limit <= 0 || max_message_bytes <= 0 {
            return Ok(vec![]);
        }
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT m.provider_uid, m.size_bytes
             FROM messages m
             LEFT JOIN message_bodies b ON b.message_id = m.id
             WHERE m.account_id = ?1
               AND m.mailbox_id = ?2
               AND m.provider_uid IS NOT NULL
               AND b.message_id IS NULL
               AND (m.size_bytes IS NULL OR m.size_bytes <= ?4)
             ORDER BY COALESCE(m.sent_at, 0) DESC, m.provider_uid DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![account_id, mailbox_id, limit, max_message_bytes],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Per-mailbox counts of stored messages that still have no body row — the
    /// durable body-hydration backlog. Uses the same missing-body predicate as
    /// [`Self::list_body_missing_uids`], so hydrated rows (including
    /// authoritative empty-body markers) drop out and the query is restart-safe
    /// without any persisted hydration cursor. Ordering across mailboxes is a
    /// caller policy (role priority, recent activity), not encoded here.
    pub fn list_hydration_backlog(
        &self,
        account_id: &str,
    ) -> Result<Vec<MailboxBodyBacklog>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT mb.id, mb.path, mb.role,
                    COUNT(*) AS missing,
                    MAX(COALESCE(m.sent_at, 0)) AS last_activity
             FROM messages m
             JOIN mailboxes mb ON mb.id = m.mailbox_id
             LEFT JOIN message_bodies b ON b.message_id = m.id
             WHERE m.account_id = ?1
               AND m.provider_uid IS NOT NULL
               AND b.message_id IS NULL
             GROUP BY mb.id, mb.path, mb.role",
        )?;
        let rows = stmt.query_map(params![account_id], |r| {
            Ok(MailboxBodyBacklog {
                mailbox_id: r.get(0)?,
                path: r.get(1)?,
                role: r.get(2)?,
                missing: r.get(3)?,
                last_activity: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Per-mailbox body-hydration progress for an account (#3631 G5), one row
    /// per mailbox (a mailbox with no messages reports 0/0). Same populations
    /// as [`Self::list_hydration_backlog`] (`provider_uid IS NOT NULL`, a
    /// `message_bodies` row means hydrated). Both counts are correlated
    /// subqueries on the partial `idx_messages_uid` (covering for the total;
    /// the hydrated count adds a PK probe into `message_bodies` per hydrated
    /// row), so no new index is needed.
    pub fn body_hydration_progress(
        &self,
        account_id: &str,
    ) -> Result<Vec<MailboxBodyProgress>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(BODY_PROGRESS_SQL)?;
        let rows = stmt.query_map(params![account_id], |r| {
            Ok(MailboxBodyProgress {
                mailbox_id: r.get(0)?,
                total: r.get(1)?,
                hydrated: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Body-hydration coverage of a search scope (#3633 G13): `account_id` /
    /// `mailbox_id` each narrow when `Some` (both `None` = every account).
    /// Counts messages with a provider UID, `missing` = no `message_bodies`
    /// row (same predicate as [`Self::list_hydration_backlog`]). The scope
    /// filters are index prefixes of the partial `idx_messages_uid`, and the
    /// body probe is a PK lookup, so it stays cheap; no new index.
    pub fn body_hydration_scope(
        &self,
        account_id: Option<&str>,
        mailbox_id: Option<&str>,
        query: Option<&str>,
    ) -> Result<BodyHydrationScope, DbError> {
        // The searched query's mailbox-targeting filters (`in:`, `folder:`,
        // `path:`, negated or not) narrow the count too, so a search that
        // excludes Trash (`-in:trash`) is not told about Trash bodies.
        let mailbox_filters = query.map_or_else(Default::default, mailbox_filters_only);
        let compiled = search_query::compile(&mailbox_filters);
        let sql = body_scope_sql(
            account_id.is_some(),
            mailbox_id.is_some(),
            &compiled.where_sql,
        );
        let mut args: Vec<Value> = Vec::new();
        args.extend(account_id.map(|a| Value::Text(a.to_string())));
        args.extend(mailbox_id.map(|m| Value::Text(m.to_string())));
        args.extend(compiled.where_params);
        let conn = self.lock_conn();
        let (total, missing) = conn.query_row(&sql, params_from_iter(args), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?.unwrap_or(0),
            ))
        })?;
        Ok(BodyHydrationScope { total, missing })
    }

    /// Apply a server-authoritative flag state to a stored message WITHOUT
    /// enqueuing a replay operation. This is the read-path counterpart to
    /// [`Self::mark_read`]/[`Self::set_flagged`] (which enqueue write ops): sync
    /// reconciliation uses it to converge local state to the server's truth, so
    /// it must never create an `operations` row (that would push the server's own
    /// state back at it in a loop).
    pub fn reconcile_message_flags(
        &self,
        account_id: &str,
        message_id: &str,
        is_read: bool,
        is_starred: bool,
    ) -> Result<(), DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        // No flag row means unread (the import default) -- matches every
        // other reader of this table (mark_read_inner,
        // list_message_headers's COALESCE(f.is_read, 0)).
        let was_read: bool = tx
            .query_row(
                "SELECT is_read FROM message_flags WHERE message_id = ?1",
                params![message_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false);
        tx.execute(
            "INSERT INTO message_flags (message_id, is_read, is_starred) VALUES (?1, ?2, ?3)
             ON CONFLICT(message_id) DO UPDATE SET
                 is_read = excluded.is_read,
                 is_starred = excluded.is_starred",
            params![message_id, is_read, is_starred],
        )?;
        // threads.unread_count depends only on is_read, not is_starred
        // -- a starred-only reconciliation costs nothing
        // extra here.
        if was_read != is_read {
            mark_threads_dirty(&tx, account_id, &[message_id])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Hard-remove a single stored message row (children cascade; the FTS index
    /// is cleaned by the `messages_before_delete_fts` trigger). Used by sync
    /// reconciliation when a message has vanished server-side (expunged/moved).
    ///
    /// NOTE: distinct from the public [`Self::delete_message`], which *moves a
    /// message to Trash* (a user action that enqueues a replayable `move` op).
    /// This purges local state only and touches no remote server.
    ///
    /// `account_id` (added for) seeds the departing message's
    /// surviving thread members as dirty BEFORE the delete and sweeps its
    /// thread if it was the last member AFTER it -- see
    /// `prepare_message_departures` for why that order matters.
    pub fn purge_message(&self, account_id: &str, message_id: &str) -> Result<(), DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let departing = [message_id.to_string()];
        let threads = prepare_message_departures(&tx, account_id, &departing)?;
        purge_ai_for_messages(&tx, "SELECT ?1", params![message_id])?;
        tx.execute("DELETE FROM messages WHERE id = ?1", params![message_id])?;
        sweep_emptied_threads(&tx, account_id, &threads)?;
        purge_orphan_thread_ai_outputs(&tx)?;
        tx.commit()?;
        Ok(())
    }

    /// Count messages in a mailbox.
    pub fn count_messages(&self, account_id: &str, mailbox_id: &str) -> Result<i64, DbError> {
        let conn = self.lock_conn();
        let n = conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE account_id = ?1 AND mailbox_id = ?2",
            params![account_id, mailbox_id],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// List message headers for a mailbox, newest first, paginated.
    pub fn list_message_headers(
        &self,
        account_id: &str,
        mailbox_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<MessageHeader>, DbError> {
        let mut headers: Vec<MessageHeader> = {
            let conn = self.lock_conn();
            let mut stmt = conn.prepare(&format!(
                "SELECT {HEADER_COLUMNS}
                 FROM messages m
                 LEFT JOIN message_flags f ON f.message_id = m.id
                 LEFT JOIN threads t ON t.id = m.thread_id
                 LEFT JOIN message_bodies b ON b.message_id = m.id
                 LEFT JOIN snoozed_messages sn ON sn.message_id = m.id
                 WHERE m.account_id = ?1 AND m.mailbox_id = ?2 AND sn.message_id IS NULL
                 ORDER BY m.sent_at DESC, m.id
                 LIMIT ?3 OFFSET ?4"
            ))?;
            let rows = stmt.query_map(
                params![account_id, mailbox_id, limit, offset],
                row_to_header,
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        self.attach_keywords(&mut headers)?;
        self.attach_recipients(&mut headers)?;
        Ok(headers)
    }

    /// Shared implementation behind `search_messages`/
    /// `search_messages_across_accounts`. `account_id: None` searches every
    /// account (S34/S39 unified search); `Some(id)` scopes to one, exactly
    /// like the pre-S39 `search_messages`. `query` is parsed with the S39
    /// grammar (`search_query::parse` + `compile`) instead of being treated
    /// as bare FTS5 text — a plain keyword query with no operators compiles
    /// to the same SQL shape as the pre-S39 code, so existing callers see no
    /// behaviour change (covered by the regression tests below).
    fn search_messages_scoped(
        &self,
        account_id: Option<&str>,
        query: &str,
        limit: i64,
    ) -> Result<Vec<MessageHeader>, DbError> {
        let compiled = search_query::compile(&search_query::parse(query));
        if compiled.match_expr.is_none() && compiled.where_sql.is_empty() {
            // Blank query, or one that degraded to nothing at all (e.g. a
            // lone "-" or pure punctuation) — nothing to search on.
            return Ok(Vec::new());
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
        if let Some(account_id) = account_id {
            predicates.push("m.account_id = ?".to_string());
            params.push(Value::Text(account_id.to_string()));
        }
        let order_clause = if compiled.ranked() {
            "ORDER BY bm25(message_fts), m.sent_at DESC"
        } else {
            "ORDER BY m.sent_at DESC, m.id"
        };

        let sql = format!(
            "SELECT m.id, m.account_id, m.mailbox_id, m.subject, m.from_name, m.from_address,
                    m.sent_at, m.snippet, m.has_attachments, COALESCE(f.is_read, 0),
                    COALESCE(t.message_count, 1), COALESCE(f.is_starred, 0),
                    b.message_id IS NOT NULL, COALESCE(t.is_muted, 0), EXISTS(SELECT 1 FROM vip_senders v WHERE v.address_key = lower(trim(m.from_address)))
             {from_clause}
             LEFT JOIN message_flags f ON f.message_id = m.id
             LEFT JOIN threads t ON t.id = m.thread_id
             LEFT JOIN message_bodies b ON b.message_id = m.id
             WHERE {predicate_sql}
             {order_clause}
             LIMIT ?",
            predicate_sql = predicates.join(" AND "),
        );
        params.push(Value::Integer(limit));

        let mut headers: Vec<MessageHeader> = {
            let conn = self.lock_conn();
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(params.iter()), row_to_header)?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        self.attach_keywords(&mut headers)?;
        self.attach_recipients(&mut headers)?;
        Ok(headers)
    }

    /// Search message headers within an account, ranked by relevance for
    /// plain keywords. `query` accepts the S39 search grammar (`from:` `to:`
    /// `cc:` `subject:` `has:attachment` `before:`/`after:` `in:` `is:`,
    /// `-` negation, quoted phrases) as well as bare keywords.
    pub fn search_messages(
        &self,
        account_id: &str,
        query: &str,
        limit: i64,
    ) -> Result<Vec<MessageHeader>, DbError> {
        self.search_messages_scoped(Some(account_id), query, limit)
    }

    /// Fetch a full message (header + addresses + body) by id.
    pub fn get_message_detail(&self, message_id: &str) -> Result<Option<MessageDetail>, DbError> {
        // The lock is held for the whole method, so all reads below are a
        // consistent snapshot (no concurrent writer can interleave).
        let conn = self.lock_conn();
        // Header + rfc822 message-id in one query (rfc822 lives on `messages`).
        let head = conn
            .query_row(
                "SELECT m.id, m.account_id, m.mailbox_id, m.subject, m.from_name, m.from_address,
                        m.sent_at, m.snippet, m.has_attachments, COALESCE(f.is_read, 0),
                        COALESCE(t.message_count, 1), COALESCE(f.is_starred, 0),
                        b.message_id IS NOT NULL, COALESCE(t.is_muted, 0), EXISTS(SELECT 1 FROM vip_senders v WHERE v.address_key = lower(trim(m.from_address))), m.rfc822_message_id,
                        m.list_unsubscribe_mailto, m.list_unsubscribe_url, m.list_unsubscribe_one_click,
                        m.thread_id
                 FROM messages m
                 LEFT JOIN message_flags f ON f.message_id = m.id
                 LEFT JOIN threads t ON t.id = m.thread_id
                 LEFT JOIN message_bodies b ON b.message_id = m.id
                 WHERE m.id = ?1",
                params![message_id],
                |r| {
                    Ok((
                        row_to_header(r)?,
                        r.get::<_, Option<String>>(15)?,
                        r.get::<_, Option<String>>(16)?,
                        r.get::<_, Option<String>>(17)?,
                        r.get::<_, i64>(18)? != 0,
                        r.get::<_, Option<String>>(19)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            mut header,
            rfc822_message_id,
            list_unsubscribe_mailto,
            list_unsubscribe_url,
            list_unsubscribe_one_click,
            thread_id,
        )) = head
        else {
            return Ok(None);
        };

        // S42: keywords, via the SAME held connection (the lock spans this
        // whole method) — never `self.attach_keywords`/`self.keywords_for_
        // messages`, which would try to re-lock and deadlock.
        {
            let mut kw_stmt =
                conn.prepare("SELECT keyword FROM message_keywords WHERE message_id = ?1")?;
            let kws: Vec<String> = kw_stmt
                .query_map(params![message_id], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            header.keywords = kws
                .into_iter()
                .filter(|k| !crate::tags::is_hidden_keyword(k))
                .collect();
        }

        let body: Option<(Option<String>, Option<String>, bool)> = conn
            .query_row(
                "SELECT text_body, sanitized_html, has_remote_content FROM message_bodies WHERE message_id = ?1",
                params![message_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0)),
            )
            .optional()?;
        let (text_body, sanitized_html, has_remote_content) = body.unwrap_or((None, None, false));

        let mut stmt = conn.prepare(
            "SELECT kind, name, address FROM message_addresses
             WHERE message_id = ?1 ORDER BY ordinal",
        )?;
        let addresses = stmt
            .query_map(params![message_id], |r| {
                Ok(Address {
                    kind: r.get(0)?,
                    name: r.get(1)?,
                    address: r.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        drop(conn);

        let attachments = self.list_message_attachments(message_id)?;

        let conn = self.lock_conn();
        let trust: Option<(String, String, String, Option<String>)> = conn
            .query_row(
                "SELECT spf, dkim, dmarc, authserv_id FROM message_trust WHERE message_id = ?1",
                params![message_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let trust = match trust {
            Some((spf, dkim, dmarc, authserv_id)) => TrustSignals {
                spf: trust_result_from_str(&spf),
                dkim: trust_result_from_str(&dkim),
                dmarc: trust_result_from_str(&dmarc),
                authserv_id,
            },
            None => TrustSignals::default(),
        };
        drop(conn);

        let calendar_invite = self.get_calendar_invite(message_id)?;

        let references: Vec<String> = {
            let conn = self.lock_conn();
            let mut stmt = conn.prepare(
                "SELECT ref_message_id FROM message_references WHERE message_id = ?1 ORDER BY ordinal",
            )?;
            let rows = stmt.query_map(params![message_id], |r| r.get(0))?;
            rows.collect::<Result<_, _>>()?
        };

        Ok(Some(MessageDetail {
            header,
            rfc822_message_id,
            addresses,
            text_body,
            sanitized_html,
            has_remote_content,
            attachments,
            trust,
            list_unsubscribe_mailto,
            list_unsubscribe_url,
            list_unsubscribe_one_click,
            thread_id,
            calendar_invite,
            references,
        }))
    }

    // --- List-Unsubscribe (S44) ----------------------------------------------

    /// Store the resolved List-Unsubscribe targets for `message_id` (called
    /// once the full RFC822 body has been parsed — see migration 0024's doc
    /// comment). Errors if the message doesn't exist.
    pub fn set_unsubscribe_targets(
        &self,
        message_id: &str,
        mailto: Option<&str>,
        url: Option<&str>,
        one_click: bool,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        let changed = conn.execute(
            "UPDATE messages
             SET list_unsubscribe_mailto = ?2, list_unsubscribe_url = ?3, list_unsubscribe_one_click = ?4
             WHERE id = ?1",
            params![message_id, mailto, url, one_click as i64],
        )?;
        if changed == 0 {
            return Err(DbError(format!("unknown message id: {message_id}")));
        }
        Ok(())
    }

    // --- unified (cross-account) reads (S34) -------------------------------

    /// Message headers across EVERY account whose mailbox has `role` (e.g.
    /// "inbox"), newest first, paginated. The core half of the sidebar's
    /// "All Inboxes" group: same query shape and stable tiebreak as
    /// `list_message_headers`, scoped by role instead of one account+mailbox.
    /// Indexed by `idx_mailboxes_role`/`idx_messages_sent_at` (migration 0023).
    pub fn list_messages_by_role(
        &self,
        role: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<MessageHeader>, DbError> {
        let mut headers: Vec<MessageHeader> = {
            let conn = self.lock_conn();
            let mut stmt = conn.prepare(&format!(
                "SELECT {HEADER_COLUMNS}
                 FROM messages m
                 JOIN mailboxes mb ON mb.id = m.mailbox_id AND mb.role = ?1
                 LEFT JOIN message_flags f ON f.message_id = m.id
                 LEFT JOIN threads t ON t.id = m.thread_id
                 LEFT JOIN message_bodies b ON b.message_id = m.id
                 LEFT JOIN snoozed_messages sn ON sn.message_id = m.id
                 WHERE sn.message_id IS NULL
                 ORDER BY m.sent_at DESC, m.id
                 LIMIT ?2 OFFSET ?3"
            ))?;
            let rows = stmt.query_map(params![role, limit, offset], row_to_header)?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        self.attach_keywords(&mut headers)?;
        self.attach_recipients(&mut headers)?;
        Ok(headers)
    }

    // --- snooze (S43) -------------------------------------------------------

    /// Snooze (or re-snooze) `message_id` until `until_epoch`. Errors if the
    /// message doesn't exist. Idempotent on the due time: re-snoozing an
    /// already-snoozed message updates it in place (one row per message, per
    /// migration 0024's PRIMARY KEY), never duplicates.
    pub fn snooze_message(&self, message_id: &str, until_epoch: i64) -> Result<(), DbError> {
        let conn = self.lock_conn();
        let changed = conn.execute(
            "INSERT INTO snoozed_messages (message_id, account_id, snoozed_until_epoch)
             SELECT id, account_id, ?2 FROM messages WHERE id = ?1
             ON CONFLICT(message_id) DO UPDATE SET snoozed_until_epoch = excluded.snoozed_until_epoch",
            params![message_id, until_epoch],
        )?;
        if changed == 0 {
            return Err(DbError(format!("unknown message id: {message_id}")));
        }
        Ok(())
    }

    /// Un-snooze `message_id` immediately ("back to inbox now"). Returns
    /// whether it had been snoozed.
    pub fn unsnooze_message(&self, message_id: &str) -> Result<bool, DbError> {
        let conn = self.lock_conn();
        let changed = conn.execute(
            "DELETE FROM snoozed_messages WHERE message_id = ?1",
            params![message_id],
        )?;
        Ok(changed > 0)
    }

    /// Every currently-snoozed message across every account, soonest due
    /// first, paginated — the core of the "Snoozed" sidebar group.
    pub fn list_snoozed_messages(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<SnoozedMessageHeader>, DbError> {
        let mut rows_out: Vec<SnoozedMessageHeader> = {
            let conn = self.lock_conn();
            let mut stmt = conn.prepare(
                "SELECT m.id, m.account_id, m.mailbox_id, m.subject, m.from_name, m.from_address,
                        m.sent_at, m.snippet, m.has_attachments, COALESCE(f.is_read, 0),
                        COALESCE(t.message_count, 1), COALESCE(f.is_starred, 0),
                        b.message_id IS NOT NULL, COALESCE(t.is_muted, 0), EXISTS(SELECT 1 FROM vip_senders v WHERE v.address_key = lower(trim(m.from_address))), sn.snoozed_until_epoch
                 FROM snoozed_messages sn
                 JOIN messages m ON m.id = sn.message_id
                 LEFT JOIN message_flags f ON f.message_id = m.id
                 LEFT JOIN threads t ON t.id = m.thread_id
                 LEFT JOIN message_bodies b ON b.message_id = m.id
                 ORDER BY sn.snoozed_until_epoch ASC, m.id
                 LIMIT ?1 OFFSET ?2",
            )?;
            let rows = stmt.query_map(params![limit, offset], |r| {
                Ok(SnoozedMessageHeader {
                    header: row_to_header(r)?,
                    snoozed_until_epoch: r.get(15)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut headers: Vec<MessageHeader> = rows_out.iter().map(|s| s.header.clone()).collect();
        self.attach_keywords(&mut headers)?;
        for (s, h) in rows_out.iter_mut().zip(headers) {
            s.header = h;
        }
        Ok(rows_out)
    }

    /// Transactionally pop every row due at or before `now_epoch`: select
    /// them, delete them, return what was released. Called on launch and on
    /// a periodic sweep (Swift `AppModel`) — SQLite, not an in-memory timer,
    /// is the only source of truth, so this is safe to call repeatedly and
    /// after any crash.
    pub fn unsnooze_due(&self, now_epoch: i64) -> Result<Vec<UnsnoozedMessage>, DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let due: Vec<UnsnoozedMessage> = {
            let mut stmt = tx.prepare(
                "SELECT message_id, account_id FROM snoozed_messages WHERE snoozed_until_epoch <= ?1",
            )?;
            let rows = stmt.query_map(params![now_epoch], |r| {
                Ok(UnsnoozedMessage {
                    message_id: r.get(0)?,
                    account_id: r.get(1)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        tx.execute(
            "DELETE FROM snoozed_messages WHERE snoozed_until_epoch <= ?1",
            params![now_epoch],
        )?;
        tx.commit()?;
        Ok(due)
    }

    /// Every thread across every account, newest activity first, paginated.
    /// The unified counterpart to `list_thread_summaries`: threads carry no
    /// `mailbox_id`/`role` (migration 0004 — a thread can span mailboxes), so
    /// unlike messages there is no role to filter on; this only drops the
    /// per-account scope. Not yet wired to any view, exactly like
    /// `list_thread_summaries`/`list_threads_json` themselves.
    pub fn list_thread_summaries_across_accounts(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<ThreadSummary>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT id, account_id, subject, last_message_at, message_count, unread_count
             FROM threads
             ORDER BY last_message_at DESC, id
             LIMIT ?1 OFFSET ?2",
        )?;
        let rows = stmt.query_map(params![limit, offset], |r| {
            Ok(ThreadSummary {
                id: r.get(0)?,
                account_id: r.get(1)?,
                subject: r.get(2)?,
                last_message_at: r.get(3)?,
                message_count: r.get(4)?,
                unread_count: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Search message headers across EVERY account — the S34 unified
    /// counterpart to `search_messages`, now also grammar-aware (S39).
    pub fn search_messages_across_accounts(
        &self,
        query: &str,
        limit: i64,
    ) -> Result<Vec<MessageHeader>, DbError> {
        self.search_messages_scoped(None, query, limit)
    }

    /// Hybrid keyword + semantic search (S53). Runs the UNCHANGED S39
    /// keyword path (`search_messages_scoped` — same SQL, same FTS5
    /// behaviour, same ordering as `search_messages`/
    /// `search_messages_across_accounts`), then, only when `query_embedding`
    /// is `Some`, re-ranks those same results by blending in cosine
    /// similarity for whichever of them already have a `message_embeddings`
    /// row (S52).
    ///
    /// `account_id: None` searches every account, exactly like
    /// `search_messages_scoped`.
    ///
    /// `query_embedding` is the caller's job to produce (the same
    /// `AiBackend::embed()` the `embed_message` background job uses) and is
    /// deliberately a plain `Option<&[f32]>`, not something this method ever
    /// computes: computing it means a network round trip to Ollama, and nothing
    /// here may block or delay keyword results on that. Pass `None` whenever
    /// the backend is unreachable, unconfigured, or the caller simply hasn't
    /// got the vector back yet — the result is then byte-for-byte what
    /// `search_messages_scoped` alone would have returned (see
    /// `hybrid_search_without_a_query_embedding_matches_keyword_only` below),
    /// so a slow/absent embedding model can never visibly delay or break
    /// search. A real integration fetches the query embedding on its own
    /// timeline (a follow-up FFI round trip, a background dispatch, or a
    /// short timeout after which `None` is used) and calls this a second time
    /// once/if it has one, rather than this method awaiting anything itself.
    ///
    /// Messages with no stored embedding — a partially-embedded mailbox, or
    /// one with none at all — simply get no semantic contribution to their
    /// score (see `combine_scores`); every keyword result is always present
    /// in the output, none are dropped, and nothing here can error on missing
    /// coverage.
    pub fn search_messages_hybrid(
        &self,
        account_id: Option<&str>,
        query: &str,
        limit: i64,
        query_embedding: Option<&[f32]>,
    ) -> Result<Vec<MessageHeader>, DbError> {
        let keyword_results = self.search_messages_scoped(account_id, query, limit)?;
        let Some(query_vec) = query_embedding else {
            return Ok(keyword_results);
        };
        if keyword_results.is_empty() {
            return Ok(keyword_results);
        }

        let ids: Vec<&str> = keyword_results.iter().map(|h| h.id.as_str()).collect();
        let embedded_rows = self.get_message_embeddings(&ids)?;
        let mut similarity_by_id: HashMap<String, f32> = HashMap::new();
        for row in embedded_rows {
            if let Some(vector) = search_query::decode_vector_f32(&row.vector, row.dimension) {
                if let Some(sim) = search_query::cosine_similarity(&vector, query_vec) {
                    similarity_by_id.insert(row.message_id, sim);
                }
                // A malformed/mismatched row (decode or cosine returned
                // `None`) contributes nothing — same as no row at all.
            }
        }

        let mut scored: Vec<(f64, usize, MessageHeader)> = keyword_results
            .into_iter()
            .enumerate()
            .map(|(rank, header)| {
                let similarity = similarity_by_id.get(&header.id).copied();
                let score = search_query::combine_scores(rank, similarity);
                (score, rank, header)
            })
            .collect();
        // Stable on ties by original keyword rank, so a fixed input (same
        // keyword results, same similarities) always yields the same order.
        scored.sort_by(|a, b| {
            // total_cmp: a total order even if a NaN ever slipped through
            // (cosine_similarity already filters non-finite, #3377).
            b.0.total_cmp(&a.0).then(a.1.cmp(&b.1))
        });
        Ok(scored.into_iter().map(|(_, _, header)| header).collect())
    }

    /// Pure semantic nearest-neighbour scan (S53-UI): given an already-
    /// computed `embedding` (the caller's job to produce — see
    /// `castellum_core::ai::embed_message_ad_hoc`), find the messages most
    /// similar to it by cosine similarity, excluding `exclude_message_id`
    /// (the anchor message a "Find similar" gesture was invoked on).
    ///
    /// Deliberately NOT `search_messages_hybrid` reused with an empty
    /// keyword query: that function blends keyword rank with similarity and
    /// requires a non-empty match expression to produce any keyword
    /// results at all, which "find similar to this message" has no
    /// equivalent for. This is a standalone nearest-neighbour query.
    ///
    /// Candidates are the most recent [`FIND_SIMILAR_SCAN_CAP`] messages
    /// (scoped to `account_id`, or every account when `None`), which keeps
    /// the brute-force scan bounded and deterministic — "similar AND
    /// recent" is usually what's wanted, and an old message dropping out of
    /// the candidate window simply won't appear, never an error. Within
    /// that candidate set, a message with no stored `message_embeddings`
    /// row is silently absent from the result (matching
    /// `get_message_embeddings`'s documented contract) rather than an
    /// error; an empty `message_embeddings` table (fresh profile, nothing
    /// embedded yet) returns an empty result. Returns `(MessageHeader,
    /// similarity)` pairs, most similar first, capped at `limit`.
    pub fn find_similar_messages(
        &self,
        account_id: Option<&str>,
        embedding: &[f32],
        exclude_message_id: &str,
        limit: i64,
    ) -> Result<Vec<(MessageHeader, f32)>, DbError> {
        self.find_similar_messages_for_model(account_id, embedding, exclude_message_id, limit, None)
    }

    /// [`Self::find_similar_messages`] restricted to vectors stored by `model`
    /// (`None` = any). Vectors from different embedding models live in
    /// different spaces: same-dimension ones would score as plausible noise and
    /// different-dimension ones are dropped, so after a model change only the
    /// current model's vectors may be compared with the query vector.
    pub fn find_similar_messages_for_model(
        &self,
        account_id: Option<&str>,
        embedding: &[f32],
        exclude_message_id: &str,
        limit: i64,
        model: Option<&str>,
    ) -> Result<Vec<(MessageHeader, f32)>, DbError> {
        let mut predicates: Vec<String> = vec!["m.id != ?".to_string()];
        let mut params: Vec<Value> = vec![Value::Text(exclude_message_id.to_string())];
        if let Some(account_id) = account_id {
            predicates.push("m.account_id = ?".to_string());
            params.push(Value::Text(account_id.to_string()));
        }
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
             ORDER BY m.sent_at DESC, m.id
             LIMIT ?",
            predicate_sql = predicates.join(" AND "),
        );
        params.push(Value::Integer(FIND_SIMILAR_SCAN_CAP));

        let candidates: Vec<MessageHeader> = {
            let conn = self.lock_conn();
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(params.iter()), row_to_header)?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        let ids: Vec<&str> = candidates.iter().map(|h| h.id.as_str()).collect();
        let embedded_rows = self.get_message_embeddings(&ids)?;
        let mut similarity_by_id: HashMap<String, f32> = HashMap::new();
        for row in embedded_rows {
            if model.is_some_and(|m| row.model_name != m) {
                continue;
            }
            if let Some(vector) = search_query::decode_vector_f32(&row.vector, row.dimension) {
                if let Some(sim) = search_query::cosine_similarity(&vector, embedding) {
                    similarity_by_id.insert(row.message_id, sim);
                }
                // A malformed/mismatched row contributes nothing — same as
                // no row at all.
            }
        }

        let mut scored: Vec<(MessageHeader, f32)> = candidates
            .into_iter()
            .filter_map(|header| {
                let similarity = similarity_by_id.get(&header.id).copied()?;
                Some((header, similarity))
            })
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored.truncate(limit.max(0) as usize);
        Ok(scored)
    }

    // --- attachment cache (S32) --------------------------------------------

    /// One cached attachment's content address and size. This type never
    /// crosses the FFI (see `castellum_core::attachments` for the JSON shape
    /// Swift actually sees) — `PartialEq`/`Eq` are derived only so tests can
    /// `assert_eq!` against it directly.
    pub fn attachment_cache_entry(
        &self,
        message_id: &str,
        ordinal: i64,
    ) -> Result<Option<AttachmentCacheEntry>, DbError> {
        let conn = self.lock_conn();
        conn.query_row(
            "SELECT content_hash, size_bytes FROM attachment_cache
             WHERE message_id = ?1 AND ordinal = ?2",
            params![message_id, ordinal],
            |r| {
                Ok(AttachmentCacheEntry {
                    content_hash: r.get(0)?,
                    size_bytes: r.get(1)?,
                })
            },
        )
        .optional()
        .map_err(DbError::from)
    }

    /// Update the LRU clock on a cache hit without changing the stored hash.
    /// A no-op (not an error) if the row doesn't exist — a caller checking
    /// `attachment_cache_entry` first and finding `None` simply won't call
    /// this.
    pub fn touch_attachment_cache_access(
        &self,
        message_id: &str,
        ordinal: i64,
        now_epoch: i64,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "UPDATE attachment_cache SET last_accessed_epoch = ?3
             WHERE message_id = ?1 AND ordinal = ?2",
            params![message_id, ordinal, now_epoch],
        )?;
        Ok(())
    }

    /// Insert or replace the cache row for (message_id, ordinal) after a
    /// successful download.
    pub fn upsert_attachment_cache_entry(
        &self,
        message_id: &str,
        ordinal: i64,
        content_hash: &str,
        size_bytes: i64,
        now_epoch: i64,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO attachment_cache
                 (message_id, ordinal, content_hash, size_bytes, last_accessed_epoch)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(message_id, ordinal) DO UPDATE SET
                 content_hash = excluded.content_hash,
                 size_bytes = excluded.size_bytes,
                 last_accessed_epoch = excluded.last_accessed_epoch",
            params![message_id, ordinal, content_hash, size_bytes, now_epoch],
        )?;
        Ok(())
    }

    /// Every distinct cached content hash with its size and most recent
    /// access across all rows sharing it, ascending by that access time
    /// (oldest first) — the eviction candidate order.
    pub fn attachment_cache_hashes_by_lru(&self) -> Result<Vec<AttachmentCacheHashUsage>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT content_hash, MAX(size_bytes), MAX(last_accessed_epoch)
             FROM attachment_cache
             GROUP BY content_hash
             ORDER BY MAX(last_accessed_epoch) ASC, content_hash ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(AttachmentCacheHashUsage {
                content_hash: r.get(0)?,
                size_bytes: r.get(1)?,
                last_accessed_epoch: r.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(DbError::from)
    }

    /// Delete every row referencing `content_hash` (across every message).
    /// The caller deletes the on-disk file separately, AFTER this succeeds
    /// (so a crash between the two never leaves a DB row pointing at a
    /// deleted file — the file would just become an orphan, cleaned up by a
    /// future eviction pass's own file-existence check, never the reverse).
    pub fn delete_attachment_cache_hash(&self, content_hash: &str) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "DELETE FROM attachment_cache WHERE content_hash = ?1",
            params![content_hash],
        )?;
        Ok(())
    }

    /// Sum of every distinct cached hash's size (not per-row — a hash shared
    /// by N rows counts once, matching one file on disk).
    pub fn total_attachment_cache_bytes(&self) -> Result<i64, DbError> {
        let conn = self.lock_conn();
        conn.query_row(
            "SELECT COALESCE(SUM(size_bytes), 0) FROM (
                 SELECT content_hash, MAX(size_bytes) AS size_bytes
                 FROM attachment_cache GROUP BY content_hash
             )",
            [],
            |r| r.get(0),
        )
        .map_err(DbError::from)
    }

    // --- threads ----------------------------------------------------------

    /// Whether a LOCAL mutation has moved this account's thread aggregates
    /// (unread_count/message_count/membership) since the last `rebuild_threads`
    /// — i.e. whether `sync`'s "nothing changed remotely" skip is unsafe to
    /// take right now. See migration 0037 and `mark_threads_dirty`. An
    /// account with no row (never mutated, or never rebuilt) reads `false`.
    pub fn threads_dirty(&self, account_id: &str) -> Result<bool, DbError> {
        let conn = self.lock_conn();
        let dirty: Option<i64> = conn
            .query_row(
                "SELECT dirty FROM thread_rebuild_state WHERE account_id = ?1",
                params![account_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(dirty == Some(1))
    }

    /// Rebuild the thread projection for an account from `messages` +
    /// `message_references`. Union-find over rfc822 message-ids: a message joins
    /// the thread of any message it references (that we have). References-based
    /// only (no subject merge) so unrelated same-subject mail isn't merged.
    /// Rebuildable and idempotent. Clears the S64 "threads dirty" marker (if
    /// set) in the SAME transaction as the rebuild, so a marker is only ever
    /// cleared alongside the rebuild that actually resolved it.
    ///
    /// Holds the thread-rebuild lock for its whole run (PR #181 review,
    /// finding 2), so it never interleaves with another rebuild, and writes
    /// in place: a thread whose id survives is UPDATEd (only when something
    /// changed), a message is re-pointed only when its thread id changed,
    /// and only thread ids that genuinely disappeared are deleted -- so
    /// `thread_extractions` (`ON DELETE CASCADE`) survive every rebuild that
    /// keeps their thread.
    pub fn rebuild_threads(&self, account_id: &str) -> Result<(), DbError> {
        let _rebuild = self.lock_thread_rebuild();
        self.rebuild_threads_full_locked(account_id)
    }

    /// The body of [`Self::rebuild_threads`]; the caller holds the
    /// thread-rebuild lock (`rebuild_threads_incremental`'s fallbacks call
    /// this directly -- the lock is not re-entrant).
    fn rebuild_threads_full_locked(&self, account_id: &str) -> Result<(), DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let msgs = load_account_thread_rows(&tx, account_id)?;
        let refs = load_account_thread_refs(&tx, account_id)?;
        let existing: Vec<String> = tx
            .prepare_cached("SELECT id FROM threads WHERE account_id = ?1")?
            .query_map(params![account_id], |r| r.get(0))?
            .collect::<Result<_, _>>()?;

        // compute_thread_groups/write_thread_group are the SAME code
        // rebuild_threads_incremental uses -- one algorithm,
        // never two implementations to keep in sync.
        let muted_anchors = load_account_muted_anchors(&tx, account_id)?;
        let groups = compute_thread_groups(&msgs, &refs, &muted_anchors);
        for group in &groups {
            write_thread_group(&tx, account_id, group)?;
        }
        // Every message of the account now points at one of `groups`, so
        // any other existing id has no member left.
        let new_ids: HashSet<&str> = groups.iter().map(|g| g.thread_id.as_str()).collect();
        let gone: Vec<String> = existing
            .into_iter()
            .filter(|id| !new_ids.contains(id.as_str()))
            .collect();
        repoint_thread_extractions(&tx, &msgs, &groups, &gone)?;
        sweep_emptied_threads(&tx, account_id, &gone)?;

        // This rebuild just recomputed every aggregate from scratch, so
        // whatever local mutation set the marker (if any) is now resolved --
        // both the boolean (migration 0037) and the per-message set
        // (migration 0039). Same transaction as the read, under the
        // connection mutex: no write can land in between.
        tx.execute(
            "DELETE FROM thread_dirty_messages WHERE account_id = ?1",
            params![account_id],
        )?;
        tx.execute(
            "INSERT INTO thread_rebuild_state (account_id, dirty, force_full) VALUES (?1, 0, 0)
             ON CONFLICT(account_id) DO UPDATE SET dirty = 0, force_full = 0",
            params![account_id],
        )?;

        tx.commit()?;
        Ok(())
    }

    /// [`Self::rebuild_threads`] but incremental: recomputes
    /// only the threads `thread_dirty_messages` says may have moved, instead
    /// of the whole account. Guaranteed to match a full rebuild's result
    /// exactly -- both paths group messages with the SAME
    /// `compute_thread_groups`, and each chunk's input is an exact closure
    /// (see `expand_thread_closure`), so the only thing that differs is
    /// which messages feed it. Returns which path ran.
    ///
    /// Concurrency (PR #181 review):
    /// - The whole call holds the thread-rebuild lock, so two rebuilds never
    ///   interleave (finding 2: `UNIQUE constraint failed: threads.id`).
    /// - It snapshots the dirty rows as (message_id, gen), then works in
    ///   chunks. Each chunk is ONE transaction under the connection mutex
    ///   that walks the closure of a batch of seeds (whole connected groups,
    ///   never a partial one), reads their current rows, upserts their
    ///   threads, re-points moved messages, sweeps thread ids that
    ///   disappeared, and clears the snapshot rows it covered whose `gen`
    ///   still matches. A reader therefore never sees a message pointing at
    ///   a missing thread row (finding 4), and a write that lands between
    ///   chunks -- even one to a message already dirty -- keeps its row for
    ///   the next pass instead of being cleared unseen (finding 3).
    /// - Writers never wait on the rebuild lock; the connection mutex is
    ///   released between chunks, so a large closure never blocks UI reads
    ///   for longer than one chunk.
    ///
    /// Falls back to a full rebuild when: nothing has ever been rebuilt for
    /// this account (first run); `thread_rebuild_state.force_full` is set (a
    /// mailbox purge or UIDVALIDITY reset -- `mark_threads_dirty_full`); the
    /// dirty set exceeds `INCREMENTAL_DIRTY_FRACTION_LIMIT` of the account's
    /// messages; or one closure walk blows past `INCREMENTAL_CLOSURE_LIMIT`
    /// (a single huge thread -- e.g. a mailing list -- can make even one
    /// dirty message's closure large while the dirty SET stays tiny). A
    /// no-op (no dirty messages, no force_full) returns
    /// [`ThreadRebuildPath::Noop`] at once, matching `sync.rs`'s existing
    /// "nothing to do" skip.
    pub fn rebuild_threads_incremental(
        &self,
        account_id: &str,
    ) -> Result<ThreadRebuildPath, DbError> {
        let _rebuild = self.lock_thread_rebuild();
        let (force_full, seeds, total_messages, ever_rebuilt) = {
            let conn = self.lock_conn();
            let force_full: bool = conn
                .query_row(
                    "SELECT force_full FROM thread_rebuild_state WHERE account_id = ?1",
                    params![account_id],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
                .map(|v| v == 1)
                .unwrap_or(false);
            let seeds: Vec<(String, i64)> = conn
                .prepare_cached(
                    "SELECT message_id, gen FROM thread_dirty_messages WHERE account_id = ?1",
                )?
                .query_map(params![account_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?;
            let total_messages: i64 = conn.query_row(
                "SELECT COUNT(*) FROM messages WHERE account_id = ?1",
                params![account_id],
                |r| r.get(0),
            )?;
            let ever_rebuilt: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM threads WHERE account_id = ?1)",
                params![account_id],
                |r| r.get::<_, i64>(0),
            )? == 1;
            (force_full, seeds, total_messages, ever_rebuilt)
        };

        if seeds.is_empty() && !force_full {
            return Ok(ThreadRebuildPath::Noop);
        }
        rebuild_gap(self, "after_snapshot");

        let dirty_fraction = if total_messages > 0 {
            seeds.len() as f64 / total_messages as f64
        } else {
            1.0
        };
        // (CI diagnosability follow-up): every path logs WHY,
        // at debug level -- so a perf regression that silently widened the
        // dirty set (or a genuine first-run/UIDVALIDITY-reset storm) is
        // distinguishable from an incremental-path slowdown just by turning
        // on `RUST_LOG=castellum_db=debug`.
        let fallback = if force_full {
            Some(FullThreadRebuildReason::ForceFull)
        } else if !ever_rebuilt {
            Some(FullThreadRebuildReason::FirstRun)
        } else if dirty_fraction > INCREMENTAL_DIRTY_FRACTION_LIMIT {
            Some(FullThreadRebuildReason::DirtyFraction)
        } else {
            None
        };
        if let Some(reason) = fallback {
            tracing::debug!(
                account_id,
                dirty_count = seeds.len(),
                total_messages,
                dirty_fraction,
                ?reason,
                "rebuild_threads_incremental: falling back to full rebuild"
            );
            self.rebuild_threads_full_locked(account_id)?;
            return Ok(ThreadRebuildPath::Full(reason));
        }

        let seed_gens: HashMap<&str, i64> = seeds.iter().map(|(id, g)| (id.as_str(), *g)).collect();
        let mut covered: HashSet<String> = HashSet::new();
        let mut next_seed = 0;
        let mut closure_total = 0usize;
        let mut chunks = 0usize;
        while next_seed < seeds.len() {
            let mut conn = self.lock_conn();
            let tx = conn.transaction()?;
            // Fill one chunk with whole connected groups: expand batches of
            // not-yet-covered seeds until the chunk reaches its target size.
            let mut chunk: HashSet<String> = HashSet::new();
            while next_seed < seeds.len() && chunk.len() < INCREMENTAL_WRITE_CHUNK {
                let mut batch: Vec<String> = Vec::new();
                while next_seed < seeds.len() && batch.len() < INCREMENTAL_SEED_BATCH {
                    let id = &seeds[next_seed].0;
                    next_seed += 1;
                    if !covered.contains(id) && !chunk.contains(id) {
                        batch.push(id.clone());
                    }
                }
                if batch.is_empty() {
                    continue;
                }
                let Some(closure) = expand_thread_closure(&tx, account_id, &batch)? else {
                    // Nothing written in this transaction yet; drop it
                    // (rolls back) and redo the account in one pass.
                    drop(tx);
                    drop(conn);
                    let reason = FullThreadRebuildReason::ClosureLimit;
                    tracing::debug!(
                        account_id,
                        dirty_count = seeds.len(),
                        limit = INCREMENTAL_CLOSURE_LIMIT,
                        ?reason,
                        "rebuild_threads_incremental: falling back to full rebuild"
                    );
                    self.rebuild_threads_full_locked(account_id)?;
                    return Ok(ThreadRebuildPath::Full(reason));
                };
                chunk.extend(closure);
            }
            if !chunk.is_empty() {
                rewrite_thread_chunk(&tx, account_id, &chunk, &seed_gens)?;
                closure_total += chunk.len();
                chunks += 1;
                covered.extend(chunk);
            }
            tx.commit()?;
            drop(conn);
            rebuild_gap(self, "between_chunks");
        }

        // Clear the account-level flag only if nothing (a write that landed
        // between chunks, a concurrent force_full) is outstanding.
        {
            let mut conn = self.lock_conn();
            let tx = conn.transaction()?;
            let outstanding: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM thread_dirty_messages WHERE account_id = ?1)
                     OR COALESCE(
                         (SELECT force_full FROM thread_rebuild_state WHERE account_id = ?1), 0)",
                params![account_id],
                |r| r.get(0),
            )?;
            if !outstanding {
                tx.execute(
                    "UPDATE thread_rebuild_state SET dirty = 0 WHERE account_id = ?1",
                    params![account_id],
                )?;
            }
            tx.commit()?;
        }
        tracing::debug!(
            account_id,
            dirty_count = seeds.len(),
            closure_size = closure_total,
            chunks,
            "rebuild_threads_incremental: incremental path"
        );
        Ok(ThreadRebuildPath::Incremental {
            seeds: seeds.len(),
            closure: closure_total,
            chunks,
        })
    }

    /// Thread summaries for an account, newest activity first, paginated.
    pub fn list_thread_summaries(
        &self,
        account_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<ThreadSummary>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT id, account_id, subject, last_message_at, message_count, unread_count
             FROM threads WHERE account_id = ?1
             ORDER BY last_message_at DESC, id
             LIMIT ?2 OFFSET ?3",
        )?;
        let rows = stmt.query_map(params![account_id, limit, offset], |r| {
            Ok(ThreadSummary {
                id: r.get(0)?,
                account_id: r.get(1)?,
                subject: r.get(2)?,
                last_message_at: r.get(3)?,
                message_count: r.get(4)?,
                unread_count: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Message headers in a thread, oldest first.
    pub fn get_thread_messages(&self, thread_id: &str) -> Result<Vec<MessageHeader>, DbError> {
        let mut headers: Vec<MessageHeader> = {
            let conn = self.lock_conn();
            let mut stmt = conn.prepare(
                "SELECT m.id, m.account_id, m.mailbox_id, m.subject, m.from_name, m.from_address,
                        m.sent_at, m.snippet, m.has_attachments, COALESCE(f.is_read, 0),
                        COALESCE(t.message_count, 1), COALESCE(f.is_starred, 0),
                        b.message_id IS NOT NULL, COALESCE(t.is_muted, 0), EXISTS(SELECT 1 FROM vip_senders v WHERE v.address_key = lower(trim(m.from_address))),
                        m.rfc822_message_id
                 FROM messages m
                 LEFT JOIN message_flags f ON f.message_id = m.id
                 LEFT JOIN threads t ON t.id = m.thread_id
                 LEFT JOIN message_bodies b ON b.message_id = m.id
                 WHERE m.thread_id = ?1
                 ORDER BY m.sent_at ASC, m.id",
            )?;
            let rows = stmt.query_map(params![thread_id], |r| {
                Ok((row_to_header(r)?, r.get::<_, Option<String>>(15)?))
            })?;
            // One entry per Message-ID (copies in several folders/labels are one
            // mail); keep the first copy (or the first one with a body), read if any
            // copy is (#3458).
            let mut seen: HashMap<String, usize> = HashMap::new();
            let mut out: Vec<MessageHeader> = Vec::new();
            for row in rows {
                let (h, rfc) = row?;
                if let Some(&i) = rfc.as_ref().and_then(|k| seen.get(k)) {
                    let read = out[i].is_read | h.is_read;
                    // Bodies are hydrated per row: prefer the copy that has
                    // one so body readers (AI summaries) get the text.
                    if h.has_body && !out[i].has_body {
                        out[i] = h;
                    }
                    out[i].is_read = read;
                    continue;
                }
                if let Some(k) = rfc {
                    seen.insert(k, out.len());
                }
                out.push(h);
            }
            out
        };
        self.attach_keywords(&mut headers)?;
        Ok(headers)
    }

    // --- operations (write path) ------------------------------------------

    /// Mark a message read/unread on EXPLICIT user intent (the Toggle
    /// Read/Unread command): apply the flag locally AND enqueue a replayable
    /// operation, atomically (never one without the other). Errors if the
    /// message id is unknown.
    ///
    /// Refused in evaluation mode with the same stable `EVALUATION_MODE_MSG`
    /// as every other explicit write, so the app can offer "Enable Writes..."
    /// instead of appearing to succeed. The auto-mark-on-open path is
    /// [`Self::mark_read_auto`], which is deliberately different — see the
    /// 2026-07-11 evaluation-mode entry in DECISIONS.md.
    pub fn mark_read(&self, message_id: &str, is_read: bool) -> Result<(), DbError> {
        self.mark_read_inner(message_id, is_read, Intent::Explicit)
    }

    /// Mark a message read as a SIDE EFFECT of opening it (auto-mark-read).
    ///
    /// The one write that is not refused in evaluation mode: it updates the
    /// LOCAL read flag optimistically but never queues remote intent, because
    /// merely reading mail must not require leaving read-only. A later sync
    /// reconciliation may revert the local flag to match the untouched server
    /// — the honest read-only outcome. An explicit toggle must NOT come
    /// through here: the user asked for a change, and silently reverting it a
    /// sync later while every neighbouring write is loudly refused is exactly
    /// the dishonesty evaluation mode exists to prevent.
    pub fn mark_read_auto(&self, message_id: &str, is_read: bool) -> Result<(), DbError> {
        self.mark_read_inner(message_id, is_read, Intent::Automatic)
    }

    fn mark_read_inner(
        &self,
        message_id: &str,
        is_read: bool,
        intent: Intent,
    ) -> Result<(), DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let owner: Option<(String, String)> = tx
            .query_row(
                "SELECT account_id, mailbox_id FROM messages WHERE id = ?1",
                params![message_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((account_id, mailbox_id)) = owner else {
            return Err(DbError(format!("unknown message id: {message_id}")));
        };
        let write_enabled = write_enabled_on(&tx, &account_id)?;
        // An explicit toggle flips a server \Seen: refused in evaluation mode.
        if intent == Intent::Explicit && !write_enabled {
            return Err(DbError(EVALUATION_MODE_MSG.into()));
        }
        // No flag row means unread (the import default).
        let was_read: bool = tx
            .query_row(
                "SELECT is_read FROM message_flags WHERE message_id = ?1",
                params![message_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false);
        tx.execute(
            "INSERT INTO message_flags (message_id, is_read) VALUES (?1, ?2)
             ON CONFLICT(message_id) DO UPDATE SET is_read = excluded.is_read",
            params![message_id, is_read],
        )?;
        if !write_enabled && was_read != is_read {
            // Local-only: no op will ever carry this to the server, and the
            // server did not change, so QRESYNC would never revert it.
            invalidate_mailbox_modseq(&tx, &mailbox_id)?;
        }
        if was_read != is_read {
            // threads.unread_count just went stale for this account,
            // whether or not the change also replays to the server (S64
            // review fix — see migration 0037).
            mark_threads_dirty(&tx, &account_id, &[message_id])?;
        }
        if write_enabled {
            let payload =
                serde_json::json!({ "message_id": message_id, "is_read": is_read }).to_string();
            tx.execute(
                "INSERT INTO operations (account_id, kind, payload_json) VALUES (?1, 'mark_read', ?2)",
                params![account_id, payload],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Mark every unread message in a mailbox read: flip the local flags AND
    /// enqueue ONE replayable `mark_all_read` op (a single bulk IMAP STORE),
    /// atomically. Returns the number of messages whose flag changed.
    ///
    /// Like `empty_trash`, the op carries a snapshot (the mailbox's
    /// UIDVALIDITY + the UIDs of exactly the rows flipped here), so replay
    /// marks only what the user saw — never mail that arrives while the op
    /// is still queued (FP27).
    pub fn mark_all_read(&self, account_id: &str, mailbox_id: &str) -> Result<usize, DbError> {
        let mut conn = self.lock_conn();
        // Bulk mutation: refused in evaluation mode (would STORE \Seen server-side).
        if !write_enabled_on(&conn, account_id)? {
            return Err(DbError(EVALUATION_MODE_MSG.into()));
        }
        let tx = conn.transaction()?;
        // The exact set of ids about to flip -- read BEFORE the bulk UPDATE
        // below, so the incremental rebuild gets the real
        // affected set instead of a row count.
        let flipping_rows: Vec<(String, Option<i64>)> = tx
            .prepare_cached(
                "SELECT m.id, m.provider_uid FROM messages m
                 LEFT JOIN message_flags f ON f.message_id = m.id
                 WHERE m.mailbox_id = ?1 AND COALESCE(f.is_read, 0) = 0",
            )?
            .query_map(params![mailbox_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let mut snapshot_uids: Vec<i64> = flipping_rows.iter().filter_map(|(_, u)| *u).collect();
        snapshot_uids.sort_unstable();
        let flipping: Vec<String> = flipping_rows.into_iter().map(|(id, _)| id).collect();
        // Rows with no flag row are unread (default) → insert is_read=1; existing
        // unread flag rows → update to 1. Both count as changed.
        let affected = tx.execute(
            "INSERT INTO message_flags (message_id, is_read)
             SELECT id, 1 FROM messages WHERE mailbox_id = ?1
             ON CONFLICT(message_id) DO UPDATE SET is_read = 1 WHERE is_read = 0",
            params![mailbox_id],
        )?;
        // Only enqueue if something actually changed (avoid empty replays).
        if affected > 0 {
            // No cursor yet is UIDVALIDITY 0, which replay refuses (fails
            // closed) exactly as it does for Empty Trash.
            let uid_validity: i64 = tx
                .query_row(
                    "SELECT uid_validity FROM sync_cursors
                     WHERE account_id = ?1 AND mailbox_id = ?2",
                    params![account_id, mailbox_id],
                    |r| r.get::<_, Option<i64>>(0),
                )
                .optional()?
                .flatten()
                .unwrap_or(0);
            let payload = serde_json::json!({
                "mailbox_id": mailbox_id,
                "uid_validity": uid_validity,
                "uids": compact_uid_ranges(&snapshot_uids),
            })
            .to_string();
            tx.execute(
                "INSERT INTO operations (account_id, kind, payload_json)
                 VALUES (?1, 'mark_all_read', ?2)",
                params![account_id, payload],
            )?;
            // threads.unread_count just went stale (S64 review fix — see
            // migration 0037; the actual ids feed the incremental engine).
            let ids: Vec<&str> = flipping.iter().map(String::as_str).collect();
            mark_threads_dirty(&tx, account_id, &ids)?;
        }
        tx.commit()?;
        Ok(affected)
    }

    /// The IMAP mailbox path for a mailbox id, for write-path replay.
    pub fn mailbox_path(&self, mailbox_id: &str) -> Result<Option<String>, DbError> {
        let conn = self.lock_conn();
        let row = conn
            .query_row(
                "SELECT path FROM mailboxes WHERE id = ?1",
                params![mailbox_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(row)
    }

    /// The IMAP (mailbox path, uid) for a message, for write-path replay.
    /// `None` if the message is unknown or has no provider uid.
    pub fn message_imap_coords(&self, message_id: &str) -> Result<Option<(String, i64)>, DbError> {
        let conn = self.lock_conn();
        let row = conn
            .query_row(
                "SELECT mb.path, m.provider_uid
                 FROM messages m JOIN mailboxes mb ON mb.id = m.mailbox_id
                 WHERE m.id = ?1",
                params![message_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)),
            )
            .optional()?;
        Ok(row.and_then(|(path, uid)| uid.map(|u| (path, u))))
    }

    /// The UIDVALIDITY the last sync recorded for `account_id`'s mailbox at
    /// `path`: the UID space every locally known UID of that mailbox belongs
    /// to. `None` when the mailbox has no sync cursor yet or the cursor holds
    /// no usable (positive) value. Write-path replay compares it with the
    /// server's current value before acting on a stored UID.
    pub fn mailbox_uid_validity(
        &self,
        account_id: &str,
        path: &str,
    ) -> Result<Option<i64>, DbError> {
        let conn = self.lock_conn();
        mailbox_uid_validity_on(&conn, account_id, path)
    }

    /// Undelivered operations for an account, oldest first: everything not yet
    /// `succeeded` or `cancelled` (i.e. queued, running, failed, or the terminal
    /// failed_permanent). The replay executor decides which of these are due
    /// (backoff/attempt cap); the UI outbox uses the same list so both a retrying
    /// `failed` op and a terminal `failed_permanent` (needs-user) op stay visible.
    /// `cancelled` ops (UIDVALIDITY reset invalidated their targets) are terminal,
    /// like `succeeded`.
    pub fn pending_operations(&self, account_id: &str) -> Result<Vec<OperationRecord>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT id, account_id, kind, payload_json, status, attempts, last_error,
                    CAST(strftime('%s', updated_at) AS INTEGER), not_before_epoch,
                    COALESCE(json_extract(payload_json, '$.scheduled'), 0)
             FROM operations
             WHERE account_id = ?1 AND status NOT IN ('succeeded', 'cancelled')
             ORDER BY id",
        )?;
        let rows = stmt.query_map(params![account_id], |r| {
            Ok(OperationRecord {
                id: r.get(0)?,
                account_id: r.get(1)?,
                kind: r.get(2)?,
                payload_json: r.get(3)?,
                status: r.get(4)?,
                attempts: r.get(5)?,
                last_error: r.get(6)?,
                updated_at_epoch: r.get(7)?,
                not_before_epoch: r.get(8)?,
                scheduled: r.get::<_, i64>(9)? != 0,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The payload JSON of operation `id` (`None` if there is no such op).
    pub fn operation_payload(&self, id: i64) -> Result<Option<String>, DbError> {
        let conn = self.lock_conn();
        Ok(conn
            .query_row(
                "SELECT payload_json FROM operations WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Record where a `move` landed (the server's COPYUID) on the op itself,
    /// so Undo can move the message back by this exact UID instead of
    /// searching for it. Keys: `dest_uid`, `dest_uid_validity`.
    pub fn record_move_result(
        &self,
        op_id: i64,
        dest_uid_validity: i64,
        dest_uid: i64,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "UPDATE operations
             SET payload_json = json_set(payload_json, '$.dest_uid', ?2, '$.dest_uid_validity', ?3)
             WHERE id = ?1 AND kind = 'move'",
            params![op_id, dest_uid, dest_uid_validity],
        )?;
        Ok(())
    }

    /// The current status of operation `id` (`None` if there is no such op).
    /// `undo_move` replay uses it to see whether the move it reverses
    /// actually reached the server.
    pub fn operation_status(&self, id: i64) -> Result<Option<String>, DbError> {
        let conn = self.lock_conn();
        Ok(conn
            .query_row(
                "SELECT status FROM operations WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Update an operation's status (executor state machine); increments the
    /// attempt counter when transitioning into `running`.
    ///
    /// Statuses `succeeded` and `cancelled` are immutable here: an executor that
    /// raced a UIDVALIDITY cancellation (op cancelled after the executor picked it
    /// up) must not resurrect the op as `succeeded`/`failed`. Losing that race is a
    /// silent no-op; an unknown id is still an error so a stuck op can't loop
    /// forever. `failed_permanent` is terminal for the executor (never retried, via
    /// `due_for_attempt`) but is deliberately left WRITABLE here so a future manual
    /// "retry" can reset it to `queued`.
    pub fn set_operation_status(
        &self,
        id: i64,
        status: &str,
        last_error: Option<&str>,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        let bump = i64::from(status == "running");
        let changed = conn.execute(
            "UPDATE operations
             SET status = ?2, attempts = attempts + ?3, last_error = ?4,
                 updated_at = datetime('now')
             WHERE id = ?1 AND status NOT IN ('succeeded', 'cancelled')",
            params![id, status, bump, last_error],
        )?;
        if changed == 0 {
            let exists: bool = conn
                .query_row(
                    "SELECT 1 FROM operations WHERE id = ?1",
                    params![id],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if !exists {
                return Err(DbError(format!("unknown operation id: {id}")));
            }
            // Already terminal: the caller lost a benign race — leave it be.
            return Ok(());
        }
        if status != "running" {
            // The attempt this launch claimed is over: drop the in-flight
            // marker (keeps the set small; the claim guard only reads it
            // while the row is `running`, so a missed drop is harmless).
            self.claimed_operations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
        }
        if matches!(status, "succeeded" | "failed_permanent") {
            // An undo that finished either way may leave copies `undo_moves`
            // dropped locally on the server (no-op for every other kind).
            queue_undo_move_refetch(&conn, id)?;
        }
        if matches!(status, "succeeded" | "cancelled") {
            // The op is done for good: reclaim its large payload immediately.
            // `failed_permanent` keeps its blob so a future manual retry can
            // still execute; the FK cascade covers eventual row deletion.
            conn.execute(
                "DELETE FROM operation_blobs WHERE operation_id = ?1",
                params![id],
            )?;
        }
        Ok(())
    }

    /// Claim an operation for one execution attempt: flips it to `running`
    /// (bumping `attempts`, clearing `last_error`) and reports whether the
    /// claim succeeded. `false` means the op reached a terminal state
    /// (user cancel, UIDVALIDITY purge, or a competing completion) after the
    /// caller's `pending_operations` snapshot was taken — the executor must
    /// SKIP it so a cancelled op can never produce its server-side effect.
    /// Unknown ids are an error so a stuck op cannot loop silently.
    ///
    /// A `running` op is claimed again only when an EARLIER launch claimed
    /// it (crash recovery: the process died mid-op). One that this open
    /// `Database` claimed is in flight right now, so a second executor — a
    /// concurrent replay, or a retry over a stale snapshot — is refused
    /// instead of applying the same op twice.
    pub fn begin_operation_attempt(&self, id: i64) -> Result<bool, DbError> {
        match self.claim_operation(id, None)? {
            Some(claimed) => Ok(claimed),
            None => Err(DbError(format!("unknown operation id: {id}"))),
        }
    }

    /// The claim behind [`Self::begin_operation_attempt`] and
    /// [`Self::begin_operation_attempt_from`]. `None` = no such op;
    /// `Some(false)` = refused (terminal, status no longer `expected_status`,
    /// or `running` under a claim this `Database` already holds).
    ///
    /// The read, the guard and the flip happen under the one `conn` lock that
    /// every in-process writer takes, so two claimers in this process cannot
    /// interleave between them.
    ///
    /// Scope: this guards ONE process. The `UPDATE`'s status predicate only
    /// makes each flip atomic; it does NOT protect against a second process
    /// on the same profile, which has its own (empty) claim set and so sees
    /// our in-flight `running` op exactly like a crash survivor and may
    /// re-claim it. Follow-up (not implemented): a profile lock or a
    /// per-launch owner token on the row, so cross-process claims are
    /// refused too.
    fn claim_operation(
        &self,
        id: i64,
        expected_status: Option<&str>,
    ) -> Result<Option<bool>, DbError> {
        let conn = self.lock_conn();
        let status: Option<String> = conn
            .query_row(
                "SELECT status FROM operations WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(status) = status else {
            return Ok(None);
        };
        if expected_status.is_some_and(|expected| expected != status) {
            return Ok(Some(false));
        }
        let mut claimed = self
            .claimed_operations
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if status == "running" && claimed.contains(&id) {
            return Ok(Some(false));
        }
        let changed = conn.execute(
            "UPDATE operations
             SET status = 'running', attempts = attempts + 1, last_error = NULL,
                 updated_at = datetime('now')
             WHERE id = ?1 AND status = ?2 AND status NOT IN ('succeeded', 'cancelled')",
            params![id, status],
        )?;
        if changed > 0 {
            claimed.insert(id);
        }
        Ok(Some(changed > 0))
    }

    /// Manually retry a failed operation (the outbox "Retry" action): resets a
    /// `failed` or `failed_permanent` op to `queued` with a fresh attempt
    /// budget. The op's blob (if any) is still present — `failed_permanent`
    /// deliberately keeps it for exactly this path. Returns whether the op was
    /// actually reset: any other status (queued/running/succeeded/cancelled)
    /// is a benign no-op so a stale UI row cannot corrupt executor state.
    /// An unknown id is an error, as in [`Self::set_operation_status`].
    pub fn retry_operation(&self, id: i64) -> Result<bool, DbError> {
        let conn = self.lock_conn();
        let changed = conn.execute(
            "UPDATE operations
             SET status = 'queued', attempts = 0, last_error = NULL,
                 updated_at = datetime('now')
             WHERE id = ?1 AND status IN ('failed', 'failed_permanent')",
            params![id],
        )?;
        if changed == 0 {
            let exists: bool = conn
                .query_row(
                    "SELECT 1 FROM operations WHERE id = ?1",
                    params![id],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if !exists {
                return Err(DbError(format!("unknown operation id: {id}")));
            }
        }
        Ok(changed > 0)
    }

    /// UIDs of `mailbox_path` (in UID space `uid_validity`) waiting for a
    /// re-fetch after a cancelled archive/delete/Empty Trash, at most `limit`,
    /// ascending. Rows recorded under another UIDVALIDITY are deleted: that
    /// UID names a different message now.
    pub fn pending_refetch_uids(
        &self,
        account_id: &str,
        mailbox_path: &str,
        uid_validity: i64,
        limit: i64,
    ) -> Result<Vec<i64>, DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "DELETE FROM pending_refetch
             WHERE account_id = ?1 AND mailbox_path = ?2 AND uid_validity <> ?3",
            params![account_id, mailbox_path, uid_validity],
        )?;
        let mut stmt = conn.prepare(
            "SELECT uid FROM pending_refetch
             WHERE account_id = ?1 AND mailbox_path = ?2 AND uid_validity = ?3
             ORDER BY uid LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![account_id, mailbox_path, uid_validity, limit],
            |r| r.get(0),
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Park arrival ids until their bodies exist (bug hunt #50): the on_sync
    /// rules with Body/Attachment conditions cannot judge a descriptor-only
    /// message. Idempotent.
    pub fn defer_rules_for(&self, account_id: &str, message_ids: &[String]) -> Result<(), DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO pending_rules (account_id, message_id)
                 SELECT ?1, id FROM messages WHERE id = ?2 AND account_id = ?1",
            )?;
            for id in message_ids {
                stmt.execute(params![account_id, id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Which of `message_ids` already have a stored body.
    pub fn messages_with_bodies(&self, message_ids: &[String]) -> Result<HashSet<String>, DbError> {
        let conn = self.lock_conn();
        let mut out = HashSet::new();
        for chunk in message_ids.chunks(IN_CLAUSE_CHUNK) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT message_id FROM message_bodies WHERE message_id IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(chunk.iter()), |r| r.get::<_, String>(0))?;
            for id in rows {
                out.insert(id?);
            }
        }
        Ok(out)
    }

    /// Which of `message_ids` have had their headers captured (MM-1c), even if
    /// none were kept.
    pub fn messages_with_header_capture(
        &self,
        message_ids: &[String],
    ) -> Result<HashSet<String>, DbError> {
        let conn = self.lock_conn();
        let mut out = HashSet::new();
        for chunk in message_ids.chunks(IN_CLAUSE_CHUNK) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT message_id FROM message_header_capture WHERE message_id IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(chunk.iter()), |r| r.get::<_, String>(0))?;
            for id in rows {
                out.insert(id?);
            }
        }
        Ok(out)
    }

    /// Take (read and remove) the parked ids that now have a body, oldest
    /// first, at most `limit`. Also forgets rows older than 7 days, which will
    /// never run (a message the hydrator skips must not wait forever).
    pub fn take_ready_deferred_rules(
        &self,
        account_id: &str,
        limit: i64,
    ) -> Result<Vec<String>, DbError> {
        self.take_ready_deferred_rules_with(account_id, limit, false)
    }

    /// As [`Self::take_ready_deferred_rules`]; with `need_headers` a parked id
    /// is ready only once its headers were captured too (MM-1c: a Header
    /// condition must not judge a message whose headers are still unknown).
    pub fn take_ready_deferred_rules_with(
        &self,
        account_id: &str,
        limit: i64,
        need_headers: bool,
    ) -> Result<Vec<String>, DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM pending_rules
             WHERE account_id = ?1 AND created_at < datetime('now', '-7 days')",
            params![account_id],
        )?;
        let ready: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT p.message_id FROM pending_rules p
                 JOIN message_bodies b ON b.message_id = p.message_id
                 WHERE p.account_id = ?1
                   AND (?3 = 0 OR p.message_id IN (SELECT message_id FROM message_header_capture))
                 ORDER BY p.created_at, p.message_id LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![account_id, limit, i64::from(need_headers)], |r| {
                    r.get(0)
                })?;
            rows.collect::<Result<_, _>>()?
        };
        for id in &ready {
            tx.execute(
                "DELETE FROM pending_rules WHERE account_id = ?1 AND message_id = ?2",
                params![account_id, id],
            )?;
        }
        tx.commit()?;
        Ok(ready)
    }

    /// Number of arrivals parked for `account_id` (tests and diagnostics).
    pub fn pending_rules_count(&self, account_id: &str) -> Result<i64, DbError> {
        let conn = self.lock_conn();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM pending_rules WHERE account_id = ?1",
            params![account_id],
            |r| r.get(0),
        )?)
    }

    /// Forget every re-fetch row of a mailbox (it is empty on the server, so
    /// there is nothing left to bring back).
    pub fn drop_pending_refetch(
        &self,
        account_id: &str,
        mailbox_path: &str,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "DELETE FROM pending_refetch WHERE account_id = ?1 AND mailbox_path = ?2",
            params![account_id, mailbox_path],
        )?;
        Ok(())
    }

    /// Forget re-fetch rows once their UIDs were attempted (found, or no
    /// longer on the server).
    pub fn clear_pending_refetch(
        &self,
        account_id: &str,
        mailbox_path: &str,
        uid_validity: i64,
        uids: &[i64],
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare_cached(
            "DELETE FROM pending_refetch
             WHERE account_id = ?1 AND mailbox_path = ?2 AND uid_validity = ?3 AND uid = ?4",
        )?;
        for uid in uids {
            stmt.execute(params![account_id, mailbox_path, uid_validity, uid])?;
        }
        Ok(())
    }

    /// Manually cancel an undelivered operation (the outbox "Cancel" action):
    /// `queued`, `failed`, or `failed_permanent` becomes terminal `cancelled`
    /// and its blob is reclaimed. A `running` op is NOT cancellable — the
    /// executor may be mid-apply and its completion write must win. Returns
    /// whether the op was cancelled; unknown id is an error.
    pub fn cancel_operation(&self, id: i64) -> Result<bool, DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "UPDATE operations
             SET status = 'cancelled', last_error = 'cancelled by user',
                 updated_at = datetime('now')
             WHERE id = ?1 AND status IN ('queued', 'failed', 'failed_permanent')",
            params![id],
        )?;
        if changed == 1 {
            // A cancelled flag op leaves its local flag applied but never
            // replayed: a local-only divergence QRESYNC cannot see.
            let (kind, payload_json): (String, String) = tx.query_row(
                "SELECT kind, payload_json FROM operations WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let payload: serde_json::Value =
                serde_json::from_str(&payload_json).unwrap_or(serde_json::Value::Null);
            let field = |key: &str| payload.get(key).and_then(|v| v.as_str());
            let mailbox_id: Option<String> = match kind.as_str() {
                "mark_read" | "flag" | "keyword" => match field("message_id") {
                    Some(message_id) => tx
                        .query_row(
                            "SELECT mailbox_id FROM messages WHERE id = ?1",
                            params![message_id],
                            |r| r.get(0),
                        )
                        .optional()?,
                    None => None,
                },
                "mark_all_read" => field("mailbox_id").map(str::to_string),
                _ => None,
            };
            if let Some(mailbox_id) = mailbox_id {
                invalidate_mailbox_modseq(&tx, &mailbox_id)?;
            }
            // Cancelling an undo frees the original to be undone again.
            if kind == "undo_move" {
                if let Some(original) = payload.get("undo_of").and_then(|v| v.as_i64()) {
                    tx.execute(
                        "UPDATE operations SET payload_json = json_remove(payload_json, '$.undone')
                         WHERE id = ?1",
                        params![original],
                    )?;
                    // The message stays where the original move put it, but
                    // `undo_moves` dropped its local row there: re-fetch it.
                    queue_undo_move_refetch(&tx, id)?;
                }
            }
            // Bug hunt #69: archive/delete and Empty Trash dropped the local row
            // up front. Cancelling leaves the message on the server with no
            // local row, and sync never re-imports below UIDNEXT, so remember
            // the UIDs for a targeted re-fetch (`pending_refetch`).
            let refetch: Option<(&str, Vec<i64>)> = match kind.as_str() {
                "move" => payload
                    .get("uid")
                    .and_then(|v| v.as_i64())
                    .zip(field("src_path"))
                    .map(|(uid, path)| (path, vec![uid])),
                "empty_trash" => field("path")
                    .zip(field("uids"))
                    .map(|(path, uids)| (path, expand_uid_ranges(uids, REFETCH_PER_OP_CAP))),
                _ => None,
            };
            let validity = payload
                .get("uid_validity")
                .and_then(|v| v.as_i64())
                .filter(|v| *v > 0);
            if let (Some((path, uids)), Some(validity)) = (refetch, validity) {
                let account: String = tx.query_row(
                    "SELECT account_id FROM operations WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )?;
                for uid in uids {
                    tx.execute(
                        "INSERT OR IGNORE INTO pending_refetch
                             (account_id, mailbox_path, uid, uid_validity)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![account, path, uid, validity],
                    )?;
                }
            }
            // FP06: cancelling a send returns its message to Drafts (same
            // statement as `undo_send`), so it is editable and deletable
            // again instead of locked in the outbox for good.
            if kind == "send" {
                if let Some(draft_id) = payload.get("draft_id").and_then(|v| v.as_i64()) {
                    tx.execute(
                        "UPDATE drafts SET status = 'draft', updated_at = datetime('now')
                         WHERE id = ?1 AND status = 'outbox'",
                        params![draft_id],
                    )?;
                }
            }
        }
        tx.commit()?;
        if changed == 0 {
            let exists: bool = conn
                .query_row(
                    "SELECT 1 FROM operations WHERE id = ?1",
                    params![id],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if !exists {
                return Err(DbError(format!("unknown operation id: {id}")));
            }
            return Ok(false);
        }
        // Terminal for good: reclaim the large payload immediately, exactly
        // like the succeeded/cancelled arm of set_operation_status.
        conn.execute(
            "DELETE FROM operation_blobs WHERE operation_id = ?1",
            params![id],
        )?;
        Ok(true)
    }

    /// Cancel every undelivered operation whose target lives in a mailbox that a
    /// UIDVALIDITY reset just purged: the server's UID space changed, so queued
    /// UIDs are stale (and could theoretically hit re-assigned UIDs). Called by
    /// the sync engine right after `delete_messages_for_mailbox` on that reset.
    ///
    /// Ops are cancelled (terminal status `cancelled`), NOT deleted, so the queue
    /// keeps an audit trail with `reason` in `last_error`; `pending_operations`
    /// excludes them, so neither executor nor the outbox ever sees them again.
    /// `reason` is recorded verbatim — callers pass a truthful, specific reason
    /// ( follow-up: this text used to be hard-coded to the
    /// UIDVALIDITY case; it is now a parameter so `apply_mailbox_selection`'s
    /// `force` path can record its own, different, truthful reason via this
    /// SAME function — see its doc comment for why the two callers share it).
    ///
    /// Matching is per-kind on the payload, never a blind substring:
    /// - `mark_read`/`flag`: `message_id` starts with `"{mailbox_id}:"`
    /// - `keyword`: only one `enqueue_move` stamped with the purged path as
    ///   its `src_path` (it would otherwise replay against the stale UID; an
    ///   unstamped one resolves through the purged local row and is moot)
    /// - `mark_all_read`: `mailbox_id` equals the purged mailbox id
    /// - `move`: `src_path` equals the purged path — SOURCE only, never
    ///   destination. A move whose DESTINATION is this mailbox is still a
    ///   perfectly valid, live action (the server folder at that path still
    ///   exists and still accepts the APPEND/COPY) — cancelling it on either
    ///   a UIDVALIDITY reset or a folder-selection edit would destroy a
    ///   pending user action for no reason: the stale-UID problem a reset
    ///   creates lives in the SOURCE coordinates, and a folder edit doesn't
    ///   create any stale-UID problem at all (see `apply_mailbox_selection`'s
    ///   doc comment — this was fixed there after a review found that a
    ///   pending Delete, i.e. a move INTO Trash, was blocking every later
    ///   folder edit while Trash stayed unticked, and would have been
    ///   cancelled under the unrelated label "the user removed Trash")
    /// - `empty_trash`/`delete_permanent`: `path` equals the purged path
    ///   (executing either would expunge server messages the user never saw,
    ///   or a message that now sits at a re-assigned UID — PR #165 round 2)
    /// - `server_draft_cleanup`: `mailbox_path` equals the purged path (its
    ///   UID is a permanent delete in the old UID space).
    ///   Hygiene only — this fires just when Drafts is a synced mailbox;
    ///   the guard that always holds is replay's UIDVALIDITY compare.
    /// - `send` ops are mailbox-independent (SMTP) and are NEVER touched.
    /// - `sent_copy` ops survive too: their payload carries the raw bytes and a
    ///   destination *path* — no stale UIDs — and an APPEND stays valid across a
    ///   UIDVALIDITY reset (they match no arm below by design).
    ///
    /// Returns the number of operations cancelled.
    pub fn cancel_operations_for_purged_mailbox(
        &self,
        account_id: &str,
        mailbox_id: &str,
        path: &str,
        reason: &str,
    ) -> Result<usize, DbError> {
        let conn = self.lock_conn();
        // Prefix with the trailing separator so "acc:INBOX" never matches a
        // message in "acc:INBOX2". substr (not LIKE) avoids %/_ wildcard issues
        // in mailbox names.
        let id_prefix = format!("{mailbox_id}:");
        let changed = conn.execute(
            "UPDATE operations
             SET status = 'cancelled',
                 last_error = ?5,
                 updated_at = datetime('now')
             WHERE account_id = ?1
               AND status NOT IN ('succeeded', 'cancelled')
               AND kind != 'send'
               AND (
                    (kind IN ('mark_read', 'flag')
                     AND substr(json_extract(payload_json, '$.message_id'),
                                1, length(?2)) = ?2)
                 OR (kind = 'mark_all_read'
                     AND json_extract(payload_json, '$.mailbox_id') = ?4)
                 OR (kind IN ('move', 'undo_move', 'keyword')
                     AND json_extract(payload_json, '$.src_path') = ?3)
                 OR (kind = 'empty_trash'
                     AND json_extract(payload_json, '$.path') = ?3)
                 OR (kind = 'delete_permanent'
                     AND json_extract(payload_json, '$.path') = ?3)
                 OR (kind = 'server_draft_cleanup'
                     AND json_extract(payload_json, '$.mailbox_path') = ?3)
               )",
            params![account_id, id_prefix, path, mailbox_id, reason],
        )?;
        Ok(changed)
    }

    /// Purge a mailbox's local rows for a UIDVALIDITY reset (or an
    /// interrupted first import with no reliable UID-space record, the tracker
    /// #2619) AND reset its sync cursor to the new UID space, in ONE
    /// transaction (Opus review of PR #180). Combines what
    /// `delete_messages_for_mailbox` (the shared `purge_mailbox_messages`,
    /// including #181's emptied-thread sweep and `force_full`) +
    /// `cancel_operations_for_purged_mailbox` used to do as two separate
    /// auto-committed calls, PLUS a cursor reset
    /// neither of them did at all: previously the cursor row (if any) was
    /// left holding the OLD `uid_validity`. If a chunked import then
    /// committed a checkpoint under the NEW `uid_validity`
    /// (`set_descriptor_checkpoint` never touches `uid_validity` on
    /// conflict) while the row's own `uid_validity` still read the OLD
    /// value, `same_uid_space` would see a mismatch on the very next pass
    /// and purge everything AGAIN — forever, since every resume attempt hits
    /// the same stale comparison. Resets to `{uid_validity: new_uid_validity,
    /// uid_next: NULL, highest_modseq: NULL, descriptor_checkpoint_uid:
    /// NULL}` so a resumed chunked import's first checkpoint write lands in
    /// the correct UID space from the start. Returns the number of
    /// cancelled operations.
    pub fn purge_mailbox_for_uid_space_reset(
        &self,
        account_id: &str,
        mailbox_id: &str,
        path: &str,
        new_uid_validity: i64,
    ) -> Result<usize, DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        // The same purge `delete_messages_for_mailbox` does (PR #181): sweep
        // the threads it emptied and force the next rebuild down the full
        // path, in this transaction.
        purge_mailbox_messages(&tx, account_id, mailbox_id)?;
        // The reset re-imports everything; UIDs recorded for a post-cancel
        // re-fetch name other messages in the new UID space.
        tx.execute(
            "DELETE FROM pending_refetch WHERE account_id = ?1 AND mailbox_path = ?2",
            params![account_id, path],
        )?;
        // Same op-cancellation SQL as `cancel_operations_for_purged_mailbox`,
        // inlined: that method opens its own connection lock, which this
        // method already holds (re-entering `lock_conn()` here would
        // deadlock, not compose transactions).
        let id_prefix = format!("{mailbox_id}:");
        let cancelled = tx.execute(
            "UPDATE operations
             SET status = 'cancelled',
                 last_error = 'cancelled: UIDVALIDITY reset purged mailbox ' || ?3,
                 updated_at = datetime('now')
             WHERE account_id = ?1
               AND status NOT IN ('succeeded', 'cancelled')
               AND kind != 'send'
               AND (
                    (kind IN ('mark_read', 'flag')
                     AND substr(json_extract(payload_json, '$.message_id'),
                                1, length(?2)) = ?2)
                 OR (kind = 'mark_all_read'
                     AND json_extract(payload_json, '$.mailbox_id') = ?4)
                 OR (kind IN ('move', 'undo_move', 'keyword')
                     AND json_extract(payload_json, '$.src_path') = ?3)
                 OR (kind = 'empty_trash'
                     AND json_extract(payload_json, '$.path') = ?3)
                 OR (kind = 'delete_permanent'
                     AND json_extract(payload_json, '$.path') = ?3)
                 OR (kind = 'server_draft_cleanup'
                     AND json_extract(payload_json, '$.mailbox_path') = ?3)
               )",
            params![account_id, id_prefix, path, mailbox_id],
        )?;
        // Deliberately UPDATE only — never INSERT a row here. A genuinely
        // first-ever import (no cursor row at all yet, not a reset of an
        // existing one) must stay cursor-less: `cursor.is_none()` is what
        // `first_ever_import` (sync.rs) reads to decide whether the S62
        // initial-history window applies, and creating a placeholder row
        // before any chunk has actually committed anything would make a
        // still-untouched mailbox look like it had already been synced.
        // When a row DOES exist (a genuine UIDVALIDITY reset, the case this
        // method exists to fix), this is exactly the reset the review asked
        // for; when none exists, it is a no-op and `set_descriptor_
        // checkpoint`'s own INSERT (on the first chunk that actually
        // commits) is what creates the row, already in the right UID space.
        tx.execute(
            "UPDATE sync_cursors
             SET uid_validity = ?3,
                 uid_next = NULL,
                 highest_modseq = NULL,
                 descriptor_checkpoint_uid = NULL,
                 updated_at = datetime('now')
             WHERE account_id = ?1 AND mailbox_id = ?2",
            params![account_id, mailbox_id, new_uid_validity],
        )?;
        // The old UID space's retry-set rows describe messages that no
        // longer mean what their UIDs say (same reasoning as the message
        // purge above) — drop them rather than retrying stale/reassigned
        // UIDs forever.
        tx.execute(
            "DELETE FROM message_decode_skips
             WHERE account_id = ?1 AND mailbox_id = ?2 AND uid_validity != ?3",
            params![account_id, mailbox_id, new_uid_validity],
        )?;
        // A local draft's server mirror in this mailbox is a UID in the old
        // UID space too: forget the UID so a later send,
        // discard or autosave cannot queue a cleanup that deletes whatever
        // now holds it. `server_message_id` is kept, so the old copy can
        // still be found by Message-ID. A mirror already recorded under the
        // new value is left alone.
        tx.execute(
            "UPDATE drafts
             SET server_uid = NULL, server_uid_validity = NULL
             WHERE account_id = ?1 AND server_mailbox_path = ?2
               AND server_uid IS NOT NULL
               AND server_uid_validity IS NOT ?3",
            params![account_id, path, new_uid_validity],
        )?;
        tx.commit()?;
        Ok(cancelled)
    }

    /// Non-terminal operations that reference `mailbox_id`/`path` as a move
    /// SOURCE (never destination — see `cancel_operations_for_purged_mailbox`'s
    /// doc comment for why), a `mark_all_read`/`mark_read`/`flag` target, or an
    /// `empty_trash`/`delete_permanent` target ( follow-up review;
    /// `delete_permanent` joined the cancel match in PR #165). Exactly the same
    /// match `cancel_operations_for_purged_mailbox` cancels — this is its
    /// read-only counting counterpart, so `apply_mailbox_selection`'s unforced
    /// precheck and its `force` cancellation (which just calls
    /// `cancel_operations_for_purged_mailbox` directly) never disagree about
    /// what counts. `send`/`sent_copy` never count (mailbox-independent).
    fn count_operations_touching_mailbox(
        &self,
        account_id: &str,
        mailbox_id: &str,
        path: &str,
    ) -> Result<i64, DbError> {
        let conn = self.lock_conn();
        let id_prefix = format!("{mailbox_id}:");
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM operations
             WHERE account_id = ?1
               AND status NOT IN ('succeeded', 'cancelled')
               AND kind != 'send'
               AND (
                    (kind IN ('mark_read', 'flag')
                     AND substr(json_extract(payload_json, '$.message_id'),
                                1, length(?2)) = ?2)
                 OR (kind = 'mark_all_read'
                     AND json_extract(payload_json, '$.mailbox_id') = ?4)
                 OR (kind IN ('move', 'undo_move', 'keyword')
                     AND json_extract(payload_json, '$.src_path') = ?3)
                 OR (kind = 'empty_trash'
                     AND json_extract(payload_json, '$.path') = ?3)
                 OR (kind = 'delete_permanent'
                     AND json_extract(payload_json, '$.path') = ?3)
                 OR (kind = 'server_draft_cleanup'
                     AND json_extract(payload_json, '$.mailbox_path') = ?3)
               )",
            params![account_id, id_prefix, path, mailbox_id],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// Read-only precheck for [`Self::apply_mailbox_selection`]'s unforced
    /// refusal ( follow-up review) — the confirmation dialog's
    /// wording source, exactly like `reset_account_blockers` is for
    /// `reset_account`. Lists, for every mailbox `keep_paths` would remove,
    /// the count of non-terminal operations still touching it
    /// (`count_operations_touching_mailbox`) — omitting any with zero.
    /// `apply_mailbox_selection` re-checks the same counts itself before
    /// actually refusing, so this is advisory, never a lock.
    pub fn apply_mailbox_selection_blockers(
        &self,
        account_id: &str,
        keep_paths: &[String],
    ) -> Result<Vec<MailboxSelectionBlocker>, DbError> {
        let existing: Vec<(String, String)> = {
            let conn = self.lock_conn();
            let mut stmt = conn.prepare("SELECT id, path FROM mailboxes WHERE account_id = ?1")?;
            let rows = stmt.query_map(params![account_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut blockers = Vec::new();
        for (mailbox_id, path) in existing {
            if keep_paths.iter().any(|p| p.eq_ignore_ascii_case(&path)) {
                continue;
            }
            let n = self.count_operations_touching_mailbox(account_id, &mailbox_id, &path)?;
            if n > 0 {
                blockers.push(MailboxSelectionBlocker {
                    path,
                    pending_operations: n,
                });
            }
        }
        Ok(blockers)
    }

    ///: apply an edited folder selection to an ALREADY-ADDED
    /// account (Settings > Accounts > Folders…), never at first-sync time —
    /// see S62/`ImapSyncConfig.mailboxes` for that. `keep_paths` is the full
    /// resolved set of paths the user still wants synced (case-insensitive
    /// match against stored paths, mirroring the sync engine's own filter
    /// match at `sync.rs`'s `mailboxes_filter` loop) and must NOT be empty —
    /// that would remove every folder, wiping the account's local mail
    /// entirely; refused with a plain error regardless of `force` (the UI's
    /// job is to never let Save reach here with nothing selected). Every
    /// OTHER locally known mailbox for this account is removed right away —
    /// deliberately not deferred to the next sync, so the sidebar reflects
    /// the edit immediately (the sync engine's own filter loop just stops
    /// re-syncing an excluded mailbox and otherwise leaves its stale local
    /// rows alone).
    ///
    /// **Pending operations are never silently cancelled** (follow-up review
    /// fix): unlike a UIDVALIDITY reset, removing a folder
    /// from the sync selection leaves server UIDs valid, so a queued
    /// `move`/`flag`/`mark_read`/`mark_all_read`/`empty_trash`/`delete_permanent` touching it
    /// would still succeed if replayed — cancelling it on removal would
    /// silently drop live user intent (e.g. drag "Old" -> INBOX, untick
    /// "Old" before replay: the message must not become invisible
    /// everywhere). So `!force` first counts operations touching each
    /// mailbox about to be removed (`count_operations_touching_mailbox` —
    /// the SAME match `cancel_operations_for_purged_mailbox` cancels: a move
    /// counts by its SOURCE only, never its destination — a second review
    /// pass found that counting the destination too made a pending Delete
    /// [a move INTO Trash] block EVERY later folder edit for as long as
    /// Trash stayed unticked, since a role mailbox's row is never deleted
    /// and so stays in `to_remove` on every call; the destination folder's
    /// server-side path still exists and the move is still perfectly valid
    /// to replay either way) and, if any exist, refuses the WHOLE call
    /// (prefixed `MAILBOX_REMOVAL_BLOCKED_MSG_PREFIX`, matched by the Swift
    /// UI like `RESET_BLOCKED_MSG_PREFIX`) with NO folder removed at all —
    /// the caller reads `apply_mailbox_selection_blockers` for the per-folder
    /// counts to show, then either waits for a sync/send to drain the queue
    /// or retries with `force: true`, which cancels those specific operations
    /// by calling `cancel_operations_for_purged_mailbox` directly with its own
    /// reason (`"cancelled: user removed folder '{path}' from sync"` —
    /// truthful and distinct from the UIDVALIDITY-purge reason) before
    /// proceeding.
    ///
    /// **Special-use folders are never deleted** (follow-up review fix): a
    /// mailbox with a `role` (inbox/sent/drafts/trash/archive/junk/…) keeps
    /// its row and role even when unticked — only its local messages, its
    /// `sync_cursors` row and its `message_decode_skips` retry-set rows
    /// (migration 0040 — they would otherwise make a later re-tick retry
    /// UIDs outside its fresh first import's window) are purged. `move_to_role`/`send_draft`/
    /// `empty_trash` resolve Trash/Sent/Drafts/etc. by ROLE and CREATE a
    /// default-named mailbox when none is found, so deleting a role row
    /// would make the very next archive/delete/send silently create a stray
    /// "Trash"/"Sent" folder instead of using the account's real one
    /// (Deleted Items, `[Gmail]/Sent Mail`, …) — Send/Delete/Archive must
    /// keep working even for an unticked special-use folder. A mailbox with
    /// NO role is removed outright: its row (and, via cascade, its messages/
    /// addresses/flags/bodies/attachments/FTS/links/`sync_cursors` row — the
    /// same FKs `reset_account` relies on) is deleted, so search and the
    /// unified inbox stop showing its mail immediately.
    ///
    /// Either way the messages go through `purge_mailbox_messages` first, in
    /// the same per-mailbox transaction as the row/cursor removal — the same
    /// purge a UIDVALIDITY reset uses (PR #181): the threads it emptied are
    /// swept at once and `force_full` marks the thread projection stale, so
    /// the next sync's rebuild-skip check can never leave a phantom
    /// conversation (or a stale count on a partly-purged one) behind.
    ///
    /// A newly ADDED folder needs no local action here at all: it has no
    /// local row (or, for a role folder, no cursor) at this point, so the
    /// next sync's ordinary discovery (`db.upsert_mailbox` in `sync.rs`)
    /// picks it up as a first-ever import, honouring the account's
    /// `initial_sync_since` window like any other new mailbox.
    ///
    /// One transaction per mailbox (a forced cancellation runs just before
    /// it), not one giant transaction like `reset_account`'s, since a partial edit
    /// (one folder removed, a DB error on the next) is still strictly safer
    /// than the pre-edit state — nothing here can revive a removed folder's
    /// stale mail. A caller passing an unknown `account_id` simply removes
    /// nothing, rather than erroring — there is nothing to prune.
    ///
    /// Returns `(removed_count, orphaned_content_hashes)`. Like
    /// `reset_account`, a removed mailbox's `attachment_cache` ROWS cascade
    /// away with its messages, but the downloaded attachment BYTES on disk
    /// (one file per content hash, shared by dedup across mailboxes/accounts)
    /// do not — and once every referencing row is gone the LRU evictor can no
    /// longer see those files either. So, per removed mailbox and BEFORE its
    /// messages go, this collects the content hashes no *other* mailbox
    /// still references; the caller (`Profile::apply_mailbox_selection_json`)
    /// unlinks them after this returns, exactly like `Profile::reset_account`
    /// does for its own orphaned hashes.
    pub fn apply_mailbox_selection(
        &self,
        account_id: &str,
        keep_paths: &[String],
        force: bool,
    ) -> Result<(usize, Vec<String>), DbError> {
        if keep_paths.is_empty() {
            return Err(DbError(
                "apply_mailbox_selection: keep_paths must not be empty — \
                 refusing to remove every synced folder"
                    .into(),
            ));
        }

        let existing: Vec<(String, String, Option<String>)> = {
            let conn = self.lock_conn();
            let mut stmt =
                conn.prepare("SELECT id, path, role FROM mailboxes WHERE account_id = ?1")?;
            let rows = stmt.query_map(params![account_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let to_remove: Vec<(String, String, Option<String>)> = existing
            .into_iter()
            .filter(|(_, path, _)| !keep_paths.iter().any(|p| p.eq_ignore_ascii_case(path)))
            .collect();

        if !force {
            let mut blocked_total = 0i64;
            let mut blocked_paths: Vec<String> = Vec::new();
            for (mailbox_id, path, _) in &to_remove {
                let n = self.count_operations_touching_mailbox(account_id, mailbox_id, path)?;
                if n > 0 {
                    blocked_total += n;
                    blocked_paths.push(path.clone());
                }
            }
            if !blocked_paths.is_empty() {
                return Err(DbError(format!(
                    "{MAILBOX_REMOVAL_BLOCKED_MSG_PREFIX} {blocked_total} pending operation(s) in {}",
                    blocked_paths.join(", ")
                )));
            }
        }

        let mut removed = 0usize;
        let mut orphaned_hashes: Vec<String> = Vec::new();
        for (mailbox_id, path, role) in &to_remove {
            if force {
                // Not in the per-folder transaction below: the cancel helper
                // takes the connection lock itself. Harmless if the purge then
                // fails — the folder's ops are cancelled but its rows remain,
                // and a retry simply finds nothing left to cancel.
                let reason = format!("cancelled: user removed folder '{path}' from sync");
                self.cancel_operations_for_purged_mailbox(account_id, mailbox_id, path, &reason)?;
            }
            let mut conn = self.lock_conn();
            let tx = conn.transaction()?;
            // A role mailbox keeps its row (see the doc comment above).
            purge_mailbox_in_tx(
                &tx,
                account_id,
                mailbox_id,
                role.is_none(),
                &mut orphaned_hashes,
            )?;
            tx.commit()?;
            drop(conn);
            removed += 1;
        }
        Ok((removed, orphaned_hashes))
    }

    /// Remove mailboxes that no longer exist on the server (bug hunt
    /// 2026-10-12): a folder deleted or renamed in another
    /// client otherwise keeps its row, cursor and every cached message in
    /// the sidebar and search forever, since it is never SELECTed again.
    ///
    /// `paths` are STORED paths, matched exactly (ids are `account:path`
    /// and IMAP names are case-sensitive). The caller — the sync pass —
    /// decides which paths qualify and owns every safeguard about trusting
    /// the server listing; this method only removes what it is given.
    ///
    /// Per mailbox, the same purge as `apply_mailbox_selection` (shared
    /// `purge_mailbox_in_tx`), with two differences that follow from the
    /// folder being GONE rather than merely unticked:
    /// - the row is deleted even when it has a role: the role is derived
    ///   again from the next LIST, and keeping a row for a folder the
    ///   server does not have would keep the ghost;
    /// - operations touching it are cancelled, never left to block: they
    ///   can only fail on replay. The reason is specific and truthful.
    ///
    /// A path with no stored row is skipped. Orphaned attachment hashes are
    /// returned for the caller to unlink, as in `apply_mailbox_selection`.
    pub fn remove_mailboxes_gone_from_server(
        &self,
        account_id: &str,
        paths: &[String],
    ) -> Result<RemovedMailboxes, DbError> {
        let mut out = RemovedMailboxes::default();
        for path in paths {
            let mailbox_id: Option<String> = {
                let conn = self.lock_conn();
                conn.query_row(
                    "SELECT id FROM mailboxes WHERE account_id = ?1 AND path = ?2",
                    params![account_id, path],
                    |r| r.get(0),
                )
                .optional()?
            };
            let Some(mailbox_id) = mailbox_id else {
                continue;
            };
            // Outside the transaction below for the same reason as the
            // forced cancellation in `apply_mailbox_selection`.
            let reason = format!("cancelled: folder '{path}' no longer exists on the server");
            out.cancelled_ops +=
                self.cancel_operations_for_purged_mailbox(account_id, &mailbox_id, path, &reason)?;
            let mut conn = self.lock_conn();
            let tx = conn.transaction()?;
            out.messages +=
                purge_mailbox_in_tx(&tx, account_id, &mailbox_id, true, &mut out.orphaned_hashes)?;
            tx.commit()?;
            drop(conn);
            out.paths.push(path.clone());
        }
        Ok(out)
    }

    // --- destructive / flag operations (slice 7d) -------------------------

    /// Toggle the local starred (`\Flagged`) state + enqueue a `flag` op.
    ///
    /// Does NOT set the S64 "threads dirty" marker (migration 0037):
    /// `is_starred` is not one of the columns `rebuild_threads` derives
    /// (unread_count/message_count/membership), so a flag toggle alone never
    /// makes the thread projection stale.
    pub fn set_flagged(&self, message_id: &str, flagged: bool) -> Result<(), DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let account_id: Option<String> = tx
            .query_row(
                "SELECT account_id FROM messages WHERE id = ?1",
                params![message_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(account_id) = account_id else {
            return Err(DbError(format!("unknown message id: {message_id}")));
        };
        // Starring flips a server \Flagged: refused in evaluation mode.
        if !write_enabled_on(&tx, &account_id)? {
            return Err(DbError(EVALUATION_MODE_MSG.into()));
        }
        tx.execute(
            "INSERT INTO message_flags (message_id, is_starred) VALUES (?1, ?2)
             ON CONFLICT(message_id) DO UPDATE SET is_starred = excluded.is_starred",
            params![message_id, flagged],
        )?;
        let payload =
            serde_json::json!({ "message_id": message_id, "flagged": flagged }).to_string();
        tx.execute(
            "INSERT INTO operations (account_id, kind, payload_json) VALUES (?1, 'flag', ?2)",
            params![account_id, payload],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Move a message to the account's Archive mailbox (Gmail/MailMate archive).
    pub fn archive_message(&self, message_id: &str) -> Result<(), DbError> {
        self.move_to_role(message_id, "archive")
    }

    /// Delete a message = move it to the account's Trash mailbox (undoable).
    ///
    /// A message already in Trash is refused: moving Trash to Trash is a
    /// no-op that would silently drop the local row. The UI routes that case
    /// through [`Database::delete_message_permanently`] after confirmation.
    pub fn delete_message(&self, message_id: &str) -> Result<(), DbError> {
        self.delete_message_op(message_id).map(|_| ())
    }

    /// [`Self::delete_message`], returning the queued `move` op's id.
    fn delete_message_op(&self, message_id: &str) -> Result<Option<i64>, DbError> {
        if self.message_trash_state(message_id)?.is_some() {
            return Err(DbError(
                "message is already in Trash; use permanent delete".into(),
            ));
        }
        self.move_to_role_op(message_id, "trash")
    }

    /// Archive, trash or move a batch of messages. Each message is its own
    /// transaction (the same partial-failure contract as calling
    /// `archive_message` / `delete_message` in a loop): one refusal does not
    /// stop the rest. Returns the queued op ids — the undo handles for
    /// [`Self::undo_moves`] — plus every refusal.
    pub fn move_messages(
        &self,
        message_ids: &[String],
        target: MoveTarget<'_>,
    ) -> Result<MoveBatchOutcome, DbError> {
        let mut out = MoveBatchOutcome::default();
        for id in message_ids {
            let res = match target {
                MoveTarget::Archive => self.move_to_role_op(id, "archive"),
                MoveTarget::Trash => self.delete_message_op(id),
                MoveTarget::Mailbox(mailbox_id) => self.move_to_mailbox_op(id, mailbox_id),
            };
            match res {
                Ok(Some(op_id)) => out.operation_ids.push(op_id),
                Ok(None) => {}
                Err(e) => {
                    let reason = e.to_string();
                    out.refused.push(MoveRefusal {
                        message_id: id.clone(),
                        evaluation_mode: reason.starts_with("evaluation mode:"),
                        reason,
                    });
                }
            }
        }
        Ok(out)
    }

    /// Queue the inverse of earlier `move` ops (archive / delete / move to
    /// folder), one `undo_move` op each, in ONE transaction for the group.
    ///
    /// Nothing local can be "un-deleted": `enqueue_move` dropped the row. The
    /// inverse is a server move back, chained behind the original (ops replay
    /// in id order), and the message returns as a NEW local row on the next
    /// sync. An op is refused when it is unknown, is not a `move` any more
    /// (Empty Trash rewrote it into a permanent delete), was cancelled, was
    /// already undone, or the account is read-only. The original is flagged
    /// `undone` so a second undo is refused, and any local copy of the message
    /// in the destination folder is dropped at once so it leaves the
    /// Trash/Archive view immediately.
    pub fn undo_moves(&self, operation_ids: &[i64]) -> Result<UndoMoveOutcome, DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let mut out = UndoMoveOutcome::default();
        for &op_id in operation_ids {
            let row: Option<(String, String, String, String)> = tx
                .query_row(
                    "SELECT account_id, kind, status, payload_json FROM operations WHERE id = ?1",
                    params![op_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            let refuse = |reason: &str, evaluation_mode: bool| UndoRefusal {
                operation_id: op_id,
                reason: reason.to_string(),
                evaluation_mode,
            };
            let Some((account_id, kind, status, payload_json)) = row else {
                out.not_undoable.push(refuse("unknown operation", false));
                continue;
            };
            let payload: serde_json::Value =
                serde_json::from_str(&payload_json).unwrap_or(serde_json::Value::Null);
            if kind != "move" {
                out.not_undoable
                    .push(refuse("this change can no longer be undone", false));
                continue;
            }
            if status == "cancelled" {
                out.not_undoable
                    .push(refuse("this change was cancelled", false));
                continue;
            }
            if payload["undone"].as_bool() == Some(true) {
                out.not_undoable
                    .push(refuse("this change was already undone", false));
                continue;
            }
            if !write_enabled_on(&tx, &account_id)? {
                out.not_undoable.push(refuse(EVALUATION_MODE_MSG, true));
                continue;
            }
            let text = |key: &str| payload[key].as_str().unwrap_or_default().to_string();
            let (src_path, dest_path) = (text("src_path"), text("dest_path"));
            let message_id = text("message_id");
            // Local copies of the message in the destination folder (if sync
            // already brought them): they leave that view now. Their UIDs go
            // into the undo payload so cancelling the undo can re-fetch them.
            let departing: Vec<(String, Option<i64>, Option<i64>)> = if message_id.is_empty() {
                Vec::new()
            } else {
                tx.prepare_cached(
                    "SELECT m.id, m.provider_uid, sc.uid_validity
                     FROM messages m JOIN mailboxes mb ON mb.id = m.mailbox_id
                     LEFT JOIN sync_cursors sc ON sc.mailbox_id = mb.id
                     WHERE m.account_id = ?1 AND mb.path = ?2 AND m.rfc822_message_id = ?3",
                )?
                .query_map(params![account_id, dest_path, message_id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?
                .collect::<Result<_, _>>()?
            };
            let dropped_dest: Vec<[i64; 2]> = departing
                .iter()
                .filter_map(|(_, uid, validity)| Some([(*uid)?, (*validity)?]))
                .collect();
            // Same key names as `move`, roles swapped, so `MovePayload`
            // parses it and every `move`-keyed guard keeps working.
            let undo_payload = serde_json::json!({
                "src_path": dest_path,
                "dest_path": src_path,
                "uid": payload["uid"],
                "uid_validity": payload["uid_validity"],
                "message_id": message_id,
                "fingerprint": payload["fingerprint"],
                "undo_of": op_id,
                "dropped_dest": dropped_dest,
            })
            .to_string();
            tx.execute(
                "INSERT INTO operations (account_id, kind, payload_json) VALUES (?1, 'undo_move', ?2)",
                params![account_id, undo_payload],
            )?;
            tx.execute(
                "UPDATE operations SET payload_json = json_set(payload_json, '$.undone', json('true'))
                 WHERE id = ?1",
                params![op_id],
            )?;
            if !departing.is_empty() {
                let departing: Vec<String> = departing.into_iter().map(|d| d.0).collect();
                let threads = prepare_message_departures(&tx, &account_id, &departing)?;
                for id in &departing {
                    purge_ai_for_messages(&tx, "SELECT ?1", params![id])?;
                    tx.execute("DELETE FROM messages WHERE id = ?1", params![id])?;
                }
                sweep_emptied_threads(&tx, &account_id, &threads)?;
                purge_orphan_thread_ai_outputs(&tx)?;
            }
            let source_mailbox: Option<String> = tx
                .query_row(
                    "SELECT id FROM mailboxes WHERE account_id = ?1 AND path = ?2",
                    params![account_id, src_path],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(mb) = source_mailbox {
                if !out.source_mailbox_ids.contains(&mb) {
                    out.source_mailbox_ids.push(mb);
                }
            }
            if !out.account_ids.contains(&account_id) {
                out.account_ids.push(account_id);
            }
            out.queued += 1;
        }
        tx.commit()?;
        Ok(out)
    }

    /// `Some((account_id, path, provider_uid, mailbox_id))` when the message
    /// lives in its account's role-`trash` mailbox, else `None`. Errors for
    /// an unknown message id.
    fn message_trash_state(&self, message_id: &str) -> Result<Option<TrashedMessage>, DbError> {
        let conn = self.lock_conn();
        let row: Option<TrashedMessageRow> = conn
            .query_row(
                "SELECT m.account_id, mb.path, m.provider_uid, mb.id, mb.role
                 FROM messages m JOIN mailboxes mb ON mb.id = m.mailbox_id
                 WHERE m.id = ?1",
                params![message_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let Some((account, path, uid, mailbox_id, role)) = row else {
            return Err(DbError(format!("unknown message id: {message_id}")));
        };
        Ok((role.as_deref() == Some("trash")).then_some((account, path, uid, mailbox_id)))
    }

    /// Permanently delete ONE message that is already in Trash: queue a
    /// `delete_permanent` op (UID STORE +FLAGS \Deleted then UID EXPUNGE at
    /// replay, guarded by the UIDVALIDITY snapshot taken here). Refuses any
    /// message outside a role-`trash` mailbox, and any message with no server
    /// UID. The local row is deliberately KEPT: it goes only when the server
    /// confirms ([`Database::finish_permanent_delete`], called by replay), so
    /// a failed expunge leaves the message visible. Idempotent: a second call
    /// while an op is pending queues nothing. The caller (UI) must have
    /// obtained an explicit confirmation first.
    pub fn delete_message_permanently(&self, message_id: &str) -> Result<(), DbError> {
        let Some((account_id, path, uid, mailbox_id)) = self.message_trash_state(message_id)?
        else {
            return Err(DbError(
                "message is not in Trash; only Trash messages can be deleted permanently".into(),
            ));
        };
        let Some(uid) = uid else {
            return Err(DbError(format!("message {message_id} has no provider uid")));
        };
        let mut conn = self.lock_conn();
        // Permanent EXPUNGE: refused in evaluation mode.
        if !write_enabled_on(&conn, &account_id)? {
            return Err(DbError(EVALUATION_MODE_MSG.into()));
        }
        let tx = conn.transaction()?;
        let pending: i64 = tx.query_row(
            "SELECT COUNT(*) FROM operations
             WHERE account_id = ?1 AND kind = 'delete_permanent'
               AND status IN ('queued', 'running', 'failed')
               AND json_extract(payload_json, '$.message_id') = ?2",
            params![account_id, message_id],
            |r| r.get(0),
        )?;
        if pending > 0 {
            return Ok(());
        }
        // UIDVALIDITY 0 = "no cursor yet": replay fails closed on it.
        let uid_validity: i64 = tx
            .query_row(
                "SELECT uid_validity FROM sync_cursors WHERE account_id = ?1 AND mailbox_id = ?2",
                params![account_id, mailbox_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten()
            .unwrap_or(0);
        let payload = serde_json::json!({
            "path": path,
            "uid_validity": uid_validity,
            "uids": uid.to_string(),
            "message_id": message_id,
        })
        .to_string();
        tx.execute(
            "INSERT INTO operations (account_id, kind, payload_json) VALUES (?1, 'delete_permanent', ?2)",
            params![account_id, payload],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Drop the local row of a message whose permanent delete the server has
    /// confirmed (replay's `delete_permanent` arm). No-op if already gone.
    pub fn finish_permanent_delete(&self, message_id: &str) -> Result<(), DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let account_id: Option<String> = tx
            .query_row(
                "SELECT account_id FROM messages WHERE id = ?1",
                params![message_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(account_id) = account_id {
            let departing = [message_id.to_string()];
            let threads = prepare_message_departures(&tx, &account_id, &departing)?;
            purge_ai_for_messages(&tx, "SELECT ?1", params![message_id])?;
            tx.execute("DELETE FROM messages WHERE id = ?1", params![message_id])?;
            sweep_emptied_threads(&tx, &account_id, &threads)?;
            purge_orphan_thread_ai_outputs(&tx)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Move a message to an arbitrary destination mailbox (not just the
    /// role-based archive/trash — S40's `RuleAction::Move` names a mailbox
    /// id directly, e.g. a rule filing mail into a specific project folder).
    /// Same enqueue path as `archive_message`/`delete_message`
    /// (`enqueue_move`, below) — never a second move implementation.
    pub fn move_message_to_mailbox(
        &self,
        message_id: &str,
        dest_mailbox_id: &str,
    ) -> Result<(), DbError> {
        self.move_to_mailbox_op(message_id, dest_mailbox_id)
            .map(|_| ())
    }

    /// [`Self::move_message_to_mailbox`], returning the queued `move` op's id
    /// (`None` when the message already lives in the destination).
    fn move_to_mailbox_op(
        &self,
        message_id: &str,
        dest_mailbox_id: &str,
    ) -> Result<Option<i64>, DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let account_id: Option<String> = tx
            .query_row(
                "SELECT account_id FROM messages WHERE id = ?1",
                params![message_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(account_id) = account_id else {
            return Err(DbError(format!("unknown message id: {message_id}")));
        };
        if !write_enabled_on(&tx, &account_id)? {
            return Err(DbError(EVALUATION_MODE_MSG.into()));
        }
        let dest_path: Option<String> = tx
            .query_row(
                "SELECT path FROM mailboxes WHERE id = ?1 AND account_id = ?2",
                params![dest_mailbox_id, account_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(dest_path) = dest_path else {
            return Err(DbError(format!(
                "unknown destination mailbox {dest_mailbox_id} for account {account_id}"
            )));
        };
        let op_id = enqueue_move(&tx, message_id, &dest_path)?;
        tx.commit()?;
        Ok(op_id)
    }

    /// Resolve the account's mailbox with `role`, enqueue a self-contained `move`
    /// op, and remove the local row (children cascade) so it leaves the source
    /// view immediately. Atomic. If the account has no mailbox for that role
    /// (e.g. a server with no Trash), create a default one locally — the provider
    /// ensures the matching server folder exists at replay time.
    fn move_to_role(&self, message_id: &str, role: &str) -> Result<(), DbError> {
        self.move_to_role_op(message_id, role).map(|_| ())
    }

    /// [`Self::move_to_role`], returning the queued `move` op's id (`None`
    /// when the message is already in that role's mailbox).
    fn move_to_role_op(&self, message_id: &str, role: &str) -> Result<Option<i64>, DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let account_id: Option<String> = tx
            .query_row(
                "SELECT account_id FROM messages WHERE id = ?1",
                params![message_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(account_id) = account_id else {
            return Err(DbError(format!("unknown message id: {message_id}")));
        };
        // Archive/delete = a server MOVE/COPY+EXPUNGE: refused in evaluation mode.
        if !write_enabled_on(&tx, &account_id)? {
            return Err(DbError(EVALUATION_MODE_MSG.into()));
        }
        let existing: Option<String> = tx
            .query_row(
                "SELECT path FROM mailboxes WHERE account_id = ?1 AND role = ?2 LIMIT 1",
                params![account_id, role],
                |r| r.get(0),
            )
            .optional()?;
        let dest_path = match existing {
            Some(p) => p,
            None => {
                let path = default_path_for_role(role)?;
                let mailbox_id = format!("{account_id}:{path}");
                tx.execute(
                    "INSERT INTO mailboxes (id, account_id, path, name, role)
                     VALUES (?1, ?2, ?3, ?3, ?4)
                     ON CONFLICT(id) DO UPDATE SET role = excluded.role",
                    params![mailbox_id, account_id, path, role],
                )?;
                path
            }
        };
        let op_id = enqueue_move(&tx, message_id, &dest_path)?;
        tx.commit()?;
        Ok(op_id)
    }

    /// Permanently empty the account's Trash: snapshot exactly what the user
    /// sees (the mailbox's UIDVALIDITY + the UID set of its local rows that
    /// carry a server UID), enqueue an `empty_trash` op with that snapshot,
    /// and drop the local Trash rows. Returns the number removed locally.
    /// Errors if there's no Trash mailbox.
    ///
    /// The snapshot is the whole point of this method: replay (see
    /// `castellum_core::replay`) must expunge exactly these UIDs, never
    /// "everything in Trash as it stands at replay time" — a message that
    /// lands in Trash between this call and a delayed replay (another
    /// client, a server-side rule, an offline queue) must survive. Local-only
    /// rows (no `provider_uid` — never synced to the server) have nothing to
    /// snapshot; they are simply deleted, as before.
    pub fn empty_trash(&self, account_id: &str) -> Result<usize, DbError> {
        let mut conn = self.lock_conn();
        // Permanent EXPUNGE: refused in evaluation mode.
        if !write_enabled_on(&conn, account_id)? {
            return Err(DbError(EVALUATION_MODE_MSG.into()));
        }
        let tx = conn.transaction()?;
        let trash: Option<(String, String)> = tx
            .query_row(
                "SELECT id, path FROM mailboxes WHERE account_id = ?1 AND role = 'trash' LIMIT 1",
                params![account_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((trash_id, trash_path)) = trash else {
            return Err(DbError(format!(
                "no Trash mailbox for account {account_id}"
            )));
        };
        // A pending move-to-Trash (archive/delete queued but not yet
        // replayed) has NOT reached Trash yet — `enqueue_move` deletes the
        // source row immediately but inserts nothing into Trash until the
        // op actually replays. Left alone, that message is invisible to the
        // snapshot below, survives on the server, and the next sync brings
        // it back locally (PR #165 review point 1). Rewrite each such op
        // into a permanent delete of its SOURCE message instead.
        rewrite_pending_moves_to_trash_as_permanent_deletes(&tx, account_id, &trash_path)?;
        // No cursor yet (mailbox never synced) is treated as UIDVALIDITY 0 —
        // a value no real server ever reports (RFC 3501: nz-number) — so a
        // stale/unknown snapshot always fails the replay-time comparison
        // closed, rather than silently matching whatever the server reports.
        let uid_validity: i64 = tx
            .query_row(
                "SELECT uid_validity FROM sync_cursors WHERE account_id = ?1 AND mailbox_id = ?2",
                params![account_id, trash_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten()
            .unwrap_or(0);
        let mut uids_stmt = tx.prepare(&format!(
            "SELECT provider_uid FROM messages
                 WHERE mailbox_id = ?1 AND provider_uid IS NOT NULL
                   AND NOT {UNDO_IN_FLIGHT_SQL}
                 ORDER BY provider_uid"
        ))?;
        let uids: Vec<i64> = uids_stmt
            .query_map(params![trash_id], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        drop(uids_stmt);
        let payload = serde_json::json!({
            "path": trash_path,
            "uid_validity": uid_validity,
            "uids": compact_uid_ranges(&uids),
        })
        .to_string();
        tx.execute(
            "INSERT INTO operations (account_id, kind, payload_json) VALUES (?1, 'empty_trash', ?2)",
            params![account_id, payload],
        )?;
        // Every id about to be removed, read BEFORE the DELETE: their
        // threads' members OUTSIDE Trash are seeded dirty as one set, and
        // after the DELETE any thread left with no member at all (the whole
        // conversation was in Trash -- the everyday "delete conversation,
        // then Empty Trash") is swept in this same transaction (PR #181
        // review, finding 1; `prepare_message_departures`).
        let departing: Vec<String> = tx
            .prepare_cached(&format!(
                "SELECT id FROM messages WHERE mailbox_id = ?1 AND NOT {UNDO_IN_FLIGHT_SQL}"
            ))?
            .query_map(params![trash_id], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let threads = prepare_message_departures(&tx, account_id, &departing)?;
        purge_ai_for_messages(
            &tx,
            &format!("SELECT id FROM messages WHERE mailbox_id = ?1 AND NOT {UNDO_IN_FLIGHT_SQL}"),
            params![trash_id],
        )?;
        let removed = tx.execute(
            &format!("DELETE FROM messages WHERE mailbox_id = ?1 AND NOT {UNDO_IN_FLIGHT_SQL}"),
            params![trash_id],
        )?;
        debug_assert_eq!(removed, departing.len());
        sweep_emptied_threads(&tx, account_id, &threads)?;
        purge_orphan_thread_ai_outputs(&tx)?;
        tx.commit()?;
        Ok(removed)
    }

    // --- drafts (compose / send) --------------------------------------------

    /// Insert (`id: None`) or update (`id: Some`) a compose draft; returns its
    /// id. Updates are only allowed while the draft's status is still `draft` —
    /// a queued (outbox) or sent draft is immutable.
    pub fn save_draft(&self, d: &NewDraft) -> Result<i64, DbError> {
        let to = addrs_to_json(&d.to)?;
        let cc = addrs_to_json(&d.cc)?;
        let bcc = addrs_to_json(&d.bcc)?;
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let id =
            match d.id {
                None => {
                    tx.execute(
                        "INSERT INTO drafts
                         (account_id, to_json, cc_json, bcc_json, subject, body_text,
                          body_html, from_email, from_name, in_reply_to, references_header)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                        params![
                            d.account_id,
                            to,
                            cc,
                            bcc,
                            d.subject,
                            d.body_text,
                            d.body_html,
                            d.from_email,
                            d.from_name,
                            d.in_reply_to,
                            d.references_header,
                        ],
                    )?;
                    tx.last_insert_rowid()
                }
                Some(id) => {
                    require_draft_status(&tx, id, "update")?;
                    // FP06: a changed From account must not keep the old
                    // account's server mirror (its UID means nothing in the new
                    // account's Drafts): queue the old copy's cleanup on the OLD
                    // account and clear the mirror columns.
                    #[allow(clippy::type_complexity)]
                let old: Option<(String, Option<String>, Option<i64>, Option<i64>)> = tx
                    .query_row(
                        "SELECT account_id, server_mailbox_path, server_uid, server_uid_validity
                         FROM drafts WHERE id = ?1",
                        params![id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .optional()?;
                    if let Some((old_account, path, uid, uid_validity)) = old {
                        if old_account != d.account_id {
                            if let (Some(path), Some(uid)) = (path, uid) {
                                Self::queue_server_draft_cleanup_tx(
                                    &tx,
                                    &old_account,
                                    &path,
                                    uid_validity,
                                    uid,
                                )?;
                            }
                            tx.execute(
                                "UPDATE drafts SET server_mailbox_path = NULL, server_uid = NULL,
                                 server_uid_validity = NULL, server_message_id = NULL
                             WHERE id = ?1",
                                params![id],
                            )?;
                            //: a save the OLD account queued and has
                            // not delivered yet (offline, slow link, backoff) would
                            // mirror stale content into the old account's Drafts
                            // and then write that account's UID onto this row.
                            // Cancel it here, with the account change, and drop
                            // its bytes. A `running` save is left to its executor
                            // (its row must not change under it): replay's
                            // pre-flight skips it if it has not appended yet, and
                            // `record_server_draft_saved` cleans up a copy that
                            // was already appended.
                            tx.execute(
                                "DELETE FROM operation_blobs WHERE operation_id IN (
                                 SELECT id FROM operations
                                 WHERE account_id = ?1 AND kind = 'server_draft_save'
                                   AND status NOT IN ('succeeded', 'cancelled', 'running')
                                   AND json_extract(payload_json, '$.draft_id') = ?2)",
                                params![old_account, id],
                            )?;
                            tx.execute(
                                "UPDATE operations
                             SET status = 'cancelled',
                                 last_error = 'cancelled: the draft moved to another account',
                                 updated_at = datetime('now')
                             WHERE account_id = ?1 AND kind = 'server_draft_save'
                               AND status NOT IN ('succeeded', 'cancelled', 'running')
                               AND json_extract(payload_json, '$.draft_id') = ?2",
                                params![old_account, id],
                            )?;
                        }
                    }
                    tx.execute(
                        "UPDATE drafts
                     SET account_id = ?2, to_json = ?3, cc_json = ?4, bcc_json = ?5,
                         subject = ?6, body_text = ?7, body_html = ?8, from_email = ?9,
                         from_name = ?10, in_reply_to = ?11, references_header = ?12,
                         updated_at = datetime('now')
                     WHERE id = ?1",
                        params![
                            id,
                            d.account_id,
                            to,
                            cc,
                            bcc,
                            d.subject,
                            d.body_text,
                            d.body_html,
                            d.from_email,
                            d.from_name,
                            d.in_reply_to,
                            d.references_header,
                        ],
                    )?;
                    id
                }
            };
        tx.commit()?;
        Ok(id)
    }

    /// All drafts for an account (any status), most recently updated first.
    pub fn list_drafts(&self, account_id: &str) -> Result<Vec<Draft>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {DRAFT_COLUMNS} FROM drafts
             WHERE account_id = ?1
             ORDER BY updated_at DESC, id DESC"
        ))?;
        let rows = stmt.query_map(params![account_id], row_to_draft)?;
        rows.map(|r| draft_from_row(r?)).collect()
    }

    /// Fetch a draft by id, or `None` if unknown.
    pub fn get_draft(&self, id: i64) -> Result<Option<Draft>, DbError> {
        let conn = self.lock_conn();
        let row = conn
            .query_row(
                &format!("SELECT {DRAFT_COLUMNS} FROM drafts WHERE id = ?1"),
                params![id],
                row_to_draft,
            )
            .optional()?;
        row.map(draft_from_row).transpose()
    }

    // --- draft attachments (S33) -------------------------------------------

    /// Add one attachment to a draft. Only while the draft's status is still
    /// `draft` (same editability rule as `save_draft`). Metadata and bytes
    /// are inserted in one transaction.
    pub fn add_draft_attachment(
        &self,
        draft_id: i64,
        filename: &str,
        content_type: &str,
        is_inline: bool,
        content_id: Option<&str>,
        bytes: &[u8],
    ) -> Result<i64, DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        require_draft_status(&tx, draft_id, "attach to")?;
        let ordinal: i64 = tx.query_row(
            "SELECT COALESCE(MAX(ordinal) + 1, 0) FROM draft_attachments WHERE draft_id = ?1",
            params![draft_id],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO draft_attachments
                 (draft_id, ordinal, filename, content_type, size_bytes, is_inline, content_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                draft_id,
                ordinal,
                filename,
                content_type,
                bytes.len() as i64,
                is_inline,
                content_id
            ],
        )?;
        let attachment_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO draft_attachment_blobs (attachment_id, bytes) VALUES (?1, ?2)",
            params![attachment_id, bytes],
        )?;
        tx.commit()?;
        Ok(attachment_id)
    }

    /// Flip one attachment of a draft between regular and inline (MM-5b slice 3).
    /// `inline = Some((content_id, content_type))` makes it an inline part with that
    /// Content-ID (and the sniffed content type); `None` makes it a regular attachment
    /// again (its Content-ID is cleared, the content type kept). Refused unless the draft
    /// is still editable (`draft`); an attachment that is not this draft's is an error.
    pub fn set_draft_attachment_inline(
        &self,
        draft_id: i64,
        attachment_id: i64,
        inline: Option<(&str, &str)>,
    ) -> Result<(), DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        require_draft_status(&tx, draft_id, "change attachments of")?;
        let changed = match inline {
            Some((content_id, content_type)) => tx.execute(
                "UPDATE draft_attachments SET is_inline = 1, content_id = ?3, content_type = ?4
                 WHERE id = ?1 AND draft_id = ?2",
                params![attachment_id, draft_id, content_id, content_type],
            )?,
            None => tx.execute(
                "UPDATE draft_attachments SET is_inline = 0, content_id = NULL
                 WHERE id = ?1 AND draft_id = ?2",
                params![attachment_id, draft_id],
            )?,
        };
        if changed == 0 {
            return Err(DbError(format!(
                "unknown attachment {attachment_id} on draft {draft_id}"
            )));
        }
        tx.commit()?;
        Ok(())
    }

    /// List a draft's attachment metadata in add order. Never loads bytes.
    pub fn list_draft_attachments(
        &self,
        draft_id: i64,
    ) -> Result<Vec<DraftAttachmentMeta>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT id, ordinal, filename, content_type, size_bytes, is_inline, content_id
             FROM draft_attachments WHERE draft_id = ?1 ORDER BY ordinal",
        )?;
        let rows = stmt.query_map(params![draft_id], |r| {
            Ok(DraftAttachmentMeta {
                id: r.get(0)?,
                ordinal: r.get(1)?,
                filename: r.get(2)?,
                content_type: r.get(3)?,
                size_bytes: r.get(4)?,
                is_inline: r.get(5)?,
                content_id: r.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(DbError::from)
    }

    /// Remove one attachment. Only while the draft's status is still `draft`.
    pub fn remove_draft_attachment(
        &self,
        draft_id: i64,
        attachment_id: i64,
    ) -> Result<(), DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        require_draft_status(&tx, draft_id, "detach from")?;
        tx.execute(
            "DELETE FROM draft_attachments WHERE id = ?1 AND draft_id = ?2",
            params![attachment_id, draft_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Bytes and metadata of the INLINE attachment of `draft_id` whose
    /// Content-ID is `content_id` (no angle brackets); `None` if there is none.
    pub fn draft_inline_attachment(
        &self,
        draft_id: i64,
        content_id: &str,
    ) -> Result<Option<(DraftAttachmentMeta, Vec<u8>)>, DbError> {
        let meta = self
            .list_draft_attachments(draft_id)?
            .into_iter()
            .find(|m| m.is_inline && m.content_id.as_deref() == Some(content_id));
        let Some(meta) = meta else { return Ok(None) };
        Ok(self
            .draft_attachment_bytes(meta.id)?
            .map(|bytes| (meta, bytes)))
    }

    /// Fetch one attachment's bytes by id; `None` if unknown.
    pub fn draft_attachment_bytes(&self, attachment_id: i64) -> Result<Option<Vec<u8>>, DbError> {
        let conn = self.lock_conn();
        conn.query_row(
            "SELECT bytes FROM draft_attachment_blobs WHERE attachment_id = ?1",
            params![attachment_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(DbError::from)
    }

    /// Delete a draft. Refused unless its status is still `draft` — an outbox
    /// draft is owned by the send queue, and a sent draft is history.
    pub fn delete_draft(&self, id: i64) -> Result<(), DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        require_draft_status(&tx, id, "delete")?;
        // A discarded draft no longer needs its server Drafts mirror (S36).
        #[allow(clippy::type_complexity)]
        let row: Option<(String, Option<String>, Option<i64>, Option<i64>)> = tx
            .query_row(
                "SELECT account_id, server_mailbox_path, server_uid, server_uid_validity
                 FROM drafts WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        if let Some((account_id, Some(path), Some(uid), uid_validity)) = row {
            Self::queue_server_draft_cleanup_tx(&tx, &account_id, &path, uid_validity, uid)?;
        }
        tx.execute("DELETE FROM drafts WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(())
    }

    /// Queue a draft for delivery: set status `outbox` AND enqueue a `send`
    /// operation, atomically. Errors if the draft is unknown or not in `draft`
    /// status (already queued or sent). Equivalent to `send_draft_delayed(id, 0)`
    /// — delivered as soon as the next drain runs.
    pub fn send_draft(&self, id: i64) -> Result<(), DbError> {
        self.send_draft_delayed(id, 0)
    }

    /// Queue a draft for delivery, with a durable undo-send window
    /// (migration 0020): `delay_seconds <= 0` behaves exactly like
    /// `send_draft` (`not_before_epoch` left `NULL`, due immediately);
    /// `delay_seconds > 0` sets `not_before_epoch = now + delay_seconds`, so
    /// the sender (`castellum_core::send`) skips the op until that time — the
    /// compose UI's grace period to call `undo_send`. Errors if the draft is
    /// unknown or not in `draft` status (already queued or sent).
    pub fn send_draft_delayed(&self, id: i64, delay_seconds: i64) -> Result<(), DbError> {
        let not_before: Option<i64> = (delay_seconds > 0).then(|| now_epoch() + delay_seconds);
        self.queue_send(id, not_before, false)
    }

    /// Send Later (HP1): queue a draft to be delivered at `send_at` (Unix epoch
    /// seconds, UTC). Same durable `send` op as the undo-send window, flagged
    /// `scheduled` in its payload so the crash rules differ: it waits through
    /// quit and relaunch (see `reconcile_undo_sends_at`). Returns an error when
    /// the draft is unknown, not a draft, or the account is in evaluation mode.
    pub fn schedule_send(&self, id: i64, send_at: i64) -> Result<(), DbError> {
        self.queue_send(id, Some(send_at), true)
    }

    /// Shared body of `send_draft_delayed` and `schedule_send`.
    fn queue_send(&self, id: i64, not_before: Option<i64>, scheduled: bool) -> Result<(), DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let row: Option<(String, String)> = tx
            .query_row(
                "SELECT account_id, status FROM drafts WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((account_id, status)) = row else {
            return Err(DbError(format!("unknown draft id: {id}")));
        };
        // Queuing a draft for SMTP delivery is a server-visible act: refused in
        // evaluation mode (the draft stays a draft, nothing is queued to send).
        if !write_enabled_on(&tx, &account_id)? {
            return Err(DbError(EVALUATION_MODE_MSG.into()));
        }
        if status != "draft" {
            return Err(DbError(format!(
                "cannot send draft {id}: status is '{status}', not 'draft'"
            )));
        }
        // A sent draft no longer needs its server Drafts mirror (S36). Reads
        // BEFORE the status flip below so it sees the mirror that's about to
        // become stale.
        let mirror: Option<(String, i64, Option<i64>)> = tx
            .query_row(
                "SELECT server_mailbox_path, server_uid, server_uid_validity FROM drafts
                 WHERE id = ?1 AND server_mailbox_path IS NOT NULL AND server_uid IS NOT NULL",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((path, uid, uid_validity)) = mirror {
            Self::queue_server_draft_cleanup_tx(&tx, &account_id, &path, uid_validity, uid)?;
            tx.execute(
                "UPDATE drafts SET server_mailbox_path = NULL, server_uid = NULL,
                     server_uid_validity = NULL, server_message_id = NULL
                 WHERE id = ?1",
                params![id],
            )?;
        }
        tx.execute(
            "UPDATE drafts SET status = 'outbox', updated_at = datetime('now') WHERE id = ?1",
            params![id],
        )?;
        let payload = if scheduled {
            serde_json::json!({ "draft_id": id, "scheduled": true })
        } else {
            serde_json::json!({ "draft_id": id })
        }
        .to_string();
        tx.execute(
            "INSERT INTO operations (account_id, kind, payload_json, not_before_epoch)
             VALUES (?1, 'send', ?2, ?3)",
            params![account_id, payload, not_before],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Undo a not-yet-submitted queued send (S25): if the draft's `send` op is
    /// still `queued` (the sender has not yet claimed it via the same
    /// status-gated `UPDATE` `begin_operation_attempt` uses — this can never
    /// race a send in progress), cancels that op and returns the draft to
    /// `draft` status, atomically. Returns whether it was undone in time;
    /// `false` (a benign no-op) means either there is no such draft/op or the
    /// sender already claimed it (`running`/terminal) — too late.
    pub fn undo_send(&self, draft_id: i64) -> Result<bool, DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        // `held` (crash-safety hold, see `reconcile_undo_sends`) is also
        // undoable: it too is not-yet-submitted, only re-labelled at startup
        // because its window's continuity couldn't be proven.
        let op_id: Option<i64> = tx
            .query_row(
                "SELECT id FROM operations
                 WHERE kind = 'send'
                   AND (status IN ('queued', 'held', 'needs_confirmation')
                        OR (status = 'failed'
                            AND COALESCE(json_extract(payload_json, '$.scheduled'), 0) = 1))
                   AND json_extract(payload_json, '$.draft_id') = ?1
                 ORDER BY id DESC LIMIT 1",
                params![draft_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(op_id) = op_id else {
            tx.commit()?;
            return Ok(false);
        };
        let changed = tx.execute(
            "UPDATE operations
             SET status = 'cancelled', last_error = 'cancelled: undo send',
                 updated_at = datetime('now')
             WHERE id = ?1 AND (status IN ('queued', 'held', 'needs_confirmation')
                                OR (status = 'failed'
                                    AND COALESCE(json_extract(payload_json, '$.scheduled'), 0) = 1))",
            params![op_id],
        )?;
        if changed == 0 {
            // Lost the race to the sender claiming it (begin_operation_attempt
            // flipped it to 'running' first) — too late, do NOT touch the draft.
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            "UPDATE drafts SET status = 'draft', updated_at = datetime('now')
             WHERE id = ?1 AND status = 'outbox'",
            params![draft_id],
        )?;
        tx.execute(
            "DELETE FROM operation_blobs WHERE operation_id = ?1",
            params![op_id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// [`Self::begin_operation_attempt`], but only while the op is still in
    /// `expected_status` (the status the caller's snapshot saw): a
    /// compare-and-swap claim. Used by the SMTP sender (OB1) so an automatic
    /// drain and an Outbox "Send Now" racing on the same op cannot both claim
    /// it and submit the message twice: whichever flips it to `running` first
    /// wins, the other sees a changed status and skips.
    ///
    /// Matching the snapshot is not enough on its own when the snapshot saw
    /// `running`: a second drain whose snapshot was taken AFTER the first
    /// drain claimed the op also sees `running`. Such an op is refused when
    /// this `Database` holds the claim (see `begin_operation_attempt`), so
    /// only a crash survivor from an earlier launch is re-claimed. An unknown
    /// id is a refused claim (`false`), not an error.
    pub fn begin_operation_attempt_from(
        &self,
        id: i64,
        expected_status: &str,
    ) -> Result<bool, DbError> {
        Ok(self
            .claim_operation(id, Some(expected_status))?
            .unwrap_or(false))
    }

    /// Retire an exhausted op to the terminal `failed_permanent`, keeping
    /// `reason` as its `last_error`, but only while it is still in
    /// `expected_status` with `attempts >= min_attempts` (the snapshot the
    /// executor judged it on). Returns whether it was retired.
    ///
    /// A `running` op this launch has claimed is NEVER retired (
    /// review): its executor is mid-attempt on its last allowed try. Retiring
    /// it would let Send Now / Retry requeue and re-claim it while the first
    /// attempt is still submitting — a second delivery. Only a crash survivor
    /// from an earlier launch, or a `failed` op at the cap, is retired. The
    /// guard and the flip run under the same `conn` lock as the claim, so a
    /// claim cannot slip in between them.
    pub fn retire_exhausted_operation(
        &self,
        id: i64,
        expected_status: &str,
        min_attempts: i64,
        reason: &str,
    ) -> Result<bool, DbError> {
        let conn = self.lock_conn();
        let mut claimed = self
            .claimed_operations
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if expected_status == "running" && claimed.contains(&id) {
            return Ok(false);
        }
        let changed = conn.execute(
            "UPDATE operations
             SET status = 'failed_permanent', last_error = ?4,
                 updated_at = datetime('now')
             WHERE id = ?1 AND status = ?2 AND attempts >= ?3",
            params![id, expected_status, min_attempts, reason],
        )?;
        if changed > 0 {
            claimed.remove(&id);
            queue_undo_move_refetch(&conn, id)?;
        }
        Ok(changed > 0)
    }

    /// Outbox "Send All" (OB1): give every failed `send` op of this account a
    /// fresh attempt budget, including permanent failures (the user asked).
    /// Held, needs-confirmation and waiting scheduled sends are NOT touched:
    /// each of those needs its own explicit decision. Returns how many ops
    /// were requeued.
    pub fn requeue_failed_sends(&self, account_id: &str) -> Result<usize, DbError> {
        let conn = self.lock_conn();
        let changed = conn.execute(
            "UPDATE operations
             SET status = 'queued', attempts = 0, last_error = NULL,
                 updated_at = datetime('now')
             WHERE account_id = ?1 AND kind = 'send'
               AND status IN ('failed', 'failed_permanent')",
            params![account_id],
        )?;
        Ok(changed)
    }

    /// Outbox "Send Now" on a stranded message (OB1): a draft still in
    /// `outbox` whose `send` op was cancelled or lost gets a new one, so it
    /// can be delivered. A no-op (returns `false`) unless the draft is in
    /// `outbox` with no live `send` op, so it can never queue a second send.
    /// Refused in evaluation mode like every other way of queuing a send.
    pub fn requeue_stranded_send(&self, draft_id: i64) -> Result<bool, DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let row: Option<(String, String)> = tx
            .query_row(
                "SELECT account_id, status FROM drafts WHERE id = ?1",
                params![draft_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((account_id, status)) = row else {
            return Ok(false);
        };
        if status != "outbox" {
            return Ok(false);
        }
        if !write_enabled_on(&tx, &account_id)? {
            return Err(DbError(EVALUATION_MODE_MSG.into()));
        }
        let live: i64 = tx.query_row(
            "SELECT COUNT(*) FROM operations
             WHERE kind = 'send' AND status NOT IN ('succeeded', 'cancelled')
               AND json_extract(payload_json, '$.draft_id') = ?1",
            params![draft_id],
            |r| r.get(0),
        )?;
        if live > 0 {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO operations (account_id, kind, payload_json)
             VALUES (?1, 'send', ?2)",
            params![
                account_id,
                serde_json::json!({ "draft_id": draft_id }).to_string()
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Release a held send for immediate delivery on the next drain: clears
    /// its time and requeues it. Covers `held` (undo-send crash hold, or a
    /// scheduled send that may have been sent), `needs_confirmation` (a
    /// scheduled send that missed its time) and a `queued` scheduled send
    /// still waiting (the outbox "Send Now"). Returns whether an op was
    /// actually released; any other status is a benign no-op.
    pub fn send_now(&self, draft_id: i64) -> Result<bool, DbError> {
        let conn = self.lock_conn();
        let changed = conn.execute(
            "UPDATE operations
             SET status = 'queued', not_before_epoch = NULL, last_error = NULL, attempts = 0,
                 updated_at = datetime('now')
             WHERE kind = 'send'
               AND (status IN ('held', 'needs_confirmation')
                    OR (status IN ('queued', 'failed')
                        AND COALESCE(json_extract(payload_json, '$.scheduled'), 0) = 1))
               AND json_extract(payload_json, '$.draft_id') = ?1",
            params![draft_id],
        )?;
        Ok(changed > 0)
    }

    /// Move a waiting scheduled send to a new time (HP1). Only a `queued`, `failed` or
    /// `needs_confirmation` scheduled send can be rescheduled: a `held` one
    /// (may have been sent) needs the user's explicit Send Now / Back to
    /// Drafts choice, and a running one is already being delivered. Returns
    /// whether an op was moved.
    pub fn reschedule_send(&self, draft_id: i64, send_at: i64) -> Result<bool, DbError> {
        let conn = self.lock_conn();
        let changed = conn.execute(
            "UPDATE operations
             SET status = 'queued', not_before_epoch = ?2, last_error = NULL, attempts = 0,
                 updated_at = datetime('now')
             WHERE kind = 'send' AND status IN ('queued', 'failed', 'needs_confirmation')
               AND COALESCE(json_extract(payload_json, '$.scheduled'), 0) = 1
               AND json_extract(payload_json, '$.draft_id') = ?1",
            params![draft_id, send_at],
        )?;
        Ok(changed > 0)
    }

    /// Crash-safety startup pass (S25 — see DECISIONS.md): holds any `queued`
    /// `send` op for `account_id` whose undo window cannot be proven to have
    /// stayed open under a continuously-live app. The backlog's hard rule is
    /// "never send after an ambiguous crash" — a delayed send must never be
    /// auto-delivered unless the app can show it was alive through the whole
    /// window.
    ///
    /// `last_clean_drain_epoch` is a durable heartbeat touched at the start of
    /// every send-drain call (`Database::touch_last_clean_drain`, called from
    /// `castellum_core::send::send_pending_with_sender`), so it advances only
    /// while the app is actually running drains. A queued `send` op whose
    /// `not_before_epoch` is AFTER that heartbeat means: the last time we can
    /// prove the app was alive, this op's window either didn't exist yet or
    /// hadn't elapsed — so whatever happened between then and now (a crash, a
    /// force-quit, or simply never running again until this restart) is
    /// unaccounted for, and the window's continuity cannot be trusted. Call
    /// this exactly ONCE per account at launch, before any drain runs — never
    /// from inside the drain loop itself, or a legitimate in-session countdown
    /// (whose window is still open and simply hasn't been proven yet because no
    /// heartbeat has ticked since it was created) would be held immediately.
    ///
    /// Returns the number of ops held.
    ///
    /// Gated to run the actual hold pass at most ONCE per account per launch
    /// (`reconciled_undo_send_accounts`, checked+claimed atomically under its
    /// own lock before touching the DB): a second call for an
    /// already-reconciled account is a guaranteed no-op. This is what makes
    /// it safe for `castellum_core::send::send_pending_with_sender` to also
    /// call this — unconditionally, before claiming any op — on every drain:
    /// normally the Swift-side launch reconcile
    /// (`AppModel.reconcileUndoSendsThenReplay`) wins the race and this call
    /// is a no-op by the time any drain runs; but if a user's immediate Send
    /// races ahead of that launch task, the send path's own call performs the
    /// hold pass itself, synchronously, before it reads `pending_operations`
    /// — closing the window where an interrupted delayed send could be
    /// claimed and delivered before it was ever reconciled. Because the gate
    /// is per-account-per-launch, a later legitimate delayed send queued
    /// (and drained) after the one-time reconcile has already run is never
    /// re-evaluated and so can never be wrongly re-held. See DECISIONS.md.
    pub fn reconcile_undo_sends(&self, account_id: &str) -> Result<usize, DbError> {
        self.reconcile_undo_sends_at(account_id, now_epoch())
    }

    /// `reconcile_undo_sends` with an injected clock (`now`, epoch seconds), so
    /// the Send Later timing rules are testable. Besides the once-per-launch
    /// hold pass it applies the late-send rule on EVERY call (it is a pure
    /// function of `now`): a `queued` scheduled send more than
    /// `LATE_SEND_GRACE_SECONDS` past its time moves to `needs_confirmation`.
    pub fn reconcile_undo_sends_at(&self, account_id: &str, now: i64) -> Result<usize, DbError> {
        self.flag_late_scheduled_sends(account_id, now)?;
        let mut done = self
            .reconciled_undo_send_accounts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !done.insert(account_id.to_string()) {
            return Ok(0);
        }
        // Held while the pass below runs: a concurrent call for the SAME
        // account (e.g. the launch reconcile and a racing send drain, on
        // different threads) blocks here until this pass has actually
        // committed, rather than merely observing "already claimed" and
        // proceeding before the hold has landed.
        self.reconcile_undo_sends_pass(account_id)
    }

    /// The raw hold pass — see `reconcile_undo_sends`, which gates this to
    /// run at most once per account per launch.
    fn reconcile_undo_sends_pass(&self, account_id: &str) -> Result<usize, DbError> {
        let conn = self.lock_conn();
        let last_clean = conn
            .query_row(
                "SELECT value FROM app_meta WHERE key = 'last_clean_drain_epoch'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0);
        let held = conn.execute(
            "UPDATE operations
             SET status = 'held',
                 last_error = 'held: interrupted before the undo window completed',
                 updated_at = datetime('now')
             WHERE account_id = ?1 AND kind = 'send' AND status = 'queued'
               AND not_before_epoch IS NOT NULL
               AND not_before_epoch > ?2
               AND COALESCE(json_extract(payload_json, '$.scheduled'), 0) = 0",
            params![account_id, last_clean],
        )?;
        // ANY send found `running` at launch was in flight when the app died
        // (HP1 for Send Later; extended it to instant and
        // undo-send sends). Whether the SMTP submission went out is
        // unknowable, so it is NEVER retried automatically — resubmitting
        // would mail the recipients a second copy. It is surfaced as `held`
        // with a "may have been sent" error and waits for the user's
        // explicit Send Now / Back to Drafts. This pass runs once per
        // launch, before any drain can claim an op, so a `running` row here
        // can only be an earlier launch's; a send this launch claims later
        // is never seen by it. (A scheduled send still waiting in `queued`
        // is deliberately not held above: it is meant to outlive the
        // process.)
        //
        // The "may have been sent" prefix is a contract with the FFI header
        // and the Swift outbox (`OutboxSends`); the scheduled wording is
        // kept verbatim for scheduled sends.
        let in_flight = conn.execute(
            "UPDATE operations
             SET status = 'held',
                 last_error = CASE
                     WHEN COALESCE(json_extract(payload_json, '$.scheduled'), 0) = 1
                     THEN 'may have been sent: the app quit while this scheduled message was being delivered'
                     ELSE 'may have been sent: the app quit while this message was being delivered'
                 END,
                 updated_at = datetime('now')
             WHERE account_id = ?1 AND kind = 'send' AND status = 'running'",
            params![account_id],
        )?;
        Ok(held + in_flight)
    }

    /// Late-send rule (HP1, DECISIONS.md 2026-10-01): a `queued` or `failed`
    /// (an attempt at its time failed, e.g. SMTP down) scheduled
    /// send whose time passed by more than `LATE_SEND_GRACE_SECONDS` before
    /// `now` is not delivered silently; it moves to `needs_confirmation`, which
    /// the UI surfaces with Send Now / Back to Drafts. Within the grace it is
    /// left alone and sends. Returns the number of ops moved.
    pub fn flag_late_scheduled_sends(&self, account_id: &str, now: i64) -> Result<usize, DbError> {
        let conn = self.lock_conn();
        Ok(conn.execute(
            "UPDATE operations
             SET status = 'needs_confirmation',
                 last_error = 'needs confirmation: the scheduled time passed while Castellum was not running',
                 updated_at = datetime('now')
             WHERE account_id = ?1 AND kind = 'send' AND status IN ('queued', 'failed')
               AND COALESCE(json_extract(payload_json, '$.scheduled'), 0) = 1
               AND not_before_epoch IS NOT NULL
               AND not_before_epoch < ?2 - ?3",
            params![account_id, now, LATE_SEND_GRACE_SECONDS],
        )?)
    }

    /// Read the durable "last clean drain" heartbeat (Unix epoch seconds), or
    /// `None` if a send drain has never run in this profile. See
    /// `reconcile_undo_sends`.
    pub fn last_clean_drain_epoch(&self) -> Result<Option<i64>, DbError> {
        let conn = self.lock_conn();
        let v: Option<String> = conn
            .query_row(
                "SELECT value FROM app_meta WHERE key = 'last_clean_drain_epoch'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.and_then(|v| v.parse::<i64>().ok()))
    }

    /// Touch the "last clean drain" heartbeat to `epoch`. Called at the start
    /// of every send-drain call — see `reconcile_undo_sends` for why the
    /// timing (start, not completion) still gives a sound liveness proof.
    pub fn touch_last_clean_drain(&self, epoch: i64) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO app_meta (key, value) VALUES ('last_clean_drain_epoch', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![epoch.to_string()],
        )?;
        Ok(())
    }

    /// Whether on-device CoreSpotlight indexing of message metadata (subject +
    /// sender only — never body/HTML/attachments) is opted in (S58, the tracker
    /// #2260). Profile-wide, not per-account: one `app_meta` row. FAIL-CLOSED,
    /// same discipline as `account_write_enabled`: a missing row (fresh
    /// profile, or a profile from before S58) reads `false` — Spotlight
    /// indexing only ever starts from an explicit opt-in, never silently.
    pub fn spotlight_indexing_enabled(&self) -> Result<bool, DbError> {
        let conn = self.lock_conn();
        let v: Option<String> = conn
            .query_row(
                "SELECT value FROM app_meta WHERE key = 'spotlight_indexing_enabled'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.as_deref() == Some("1"))
    }

    /// Persist the CoreSpotlight indexing opt-in. The Swift side is the
    /// single caller: flipping this on/off is what drives the one-off
    /// backfill (see `SpotlightIndexer.backfillAll` in the Swift layer) or
    /// the domain-scoped `CSSearchableIndex` removal on the way off — this
    /// method itself only persists the flag, no indexing side effects live
    /// in Rust (CoreSpotlight has no C ABI; it's Apple-framework-only).
    pub fn set_spotlight_indexing_enabled(&self, enabled: bool) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO app_meta (key, value) VALUES ('spotlight_indexing_enabled', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![if enabled { "1" } else { "0" }],
        )?;
        Ok(())
    }

    /// Mark a draft delivered (status `sent`). Called by the send executor
    /// after a successful SMTP submission. Errors if the id is unknown.
    pub fn mark_draft_sent(&self, id: i64) -> Result<(), DbError> {
        let conn = self.lock_conn();
        let changed = conn.execute(
            "UPDATE drafts SET status = 'sent', updated_at = datetime('now') WHERE id = ?1",
            params![id],
        )?;
        if changed == 0 {
            return Err(DbError(format!("unknown draft id: {id}")));
        }
        Ok(())
    }

    /// `mark_draft_sent` + enqueue a durable `sent_copy` op, atomically.
    ///
    /// Real IMAP servers do not file SMTP submissions into Sent — the client
    /// must APPEND its own copy. The op payload carries only small routing
    /// fields (Sent path and the generated Message-ID for the provider's
    /// idempotency guard); the delivered RFC822 bytes go to `operation_blobs`
    /// keyed by the op id, IN THE SAME transaction, so the copy still survives
    /// crashes and restarts while `pending_operations` stays O(op count), not
    /// O(total message bytes). The replayer fetches the blob at apply time.
    ///
    /// The Sent mailbox is resolved by `role = 'sent'`; when the account has
    /// none, a default "Sent" is created locally (same pattern as Trash/Archive
    /// in `move_to_role`) and the provider creates the server folder at replay
    /// time. One transaction: a crash can never record the delivery without
    /// queueing the copy (bytes included), or vice versa.
    pub fn mark_draft_sent_with_copy(
        &self,
        id: i64,
        rfc822_message_id: &str,
        raw: &[u8],
    ) -> Result<(), DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let account_id: Option<String> = tx
            .query_row(
                "SELECT account_id FROM drafts WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(account_id) = account_id else {
            return Err(DbError(format!("unknown draft id: {id}")));
        };
        tx.execute(
            "UPDATE drafts SET status = 'sent', updated_at = datetime('now') WHERE id = ?1",
            params![id],
        )?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT path FROM mailboxes WHERE account_id = ?1 AND role = 'sent' LIMIT 1",
                params![account_id],
                |r| r.get(0),
            )
            .optional()?;
        let dest_path = match existing {
            Some(p) => p,
            None => {
                let path = default_path_for_role("sent")?;
                let mailbox_id = format!("{account_id}:{path}");
                tx.execute(
                    "INSERT INTO mailboxes (id, account_id, path, name, role)
                     VALUES (?1, ?2, ?3, ?3, 'sent')
                     ON CONFLICT(id) DO UPDATE SET role = excluded.role",
                    params![mailbox_id, account_id, path],
                )?;
                path
            }
        };
        let payload = serde_json::json!({
            "dest_path": dest_path,
            "message_id": rfc822_message_id,
        })
        .to_string();
        tx.execute(
            "INSERT INTO operations (account_id, kind, payload_json) VALUES (?1, 'sent_copy', ?2)",
            params![account_id, payload],
        )?;
        let op_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO operation_blobs (operation_id, bytes) VALUES (?1, ?2)",
            params![op_id, raw],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Enqueue a `server_draft_save` op for `draft_id`: resolve/create the
    /// account's Drafts-role mailbox (same pattern as
    /// `mark_draft_sent_with_copy`'s Sent resolution), store `raw` in
    /// `operation_blobs`, and queue the op. A benign no-op (`Ok(())`, no op
    /// queued) when the draft has already left `draft` status — the user
    /// sent or discarded it before this debounced save fired; the content is
    /// moot. `message_id` is the RFC822 Message-ID already baked into `raw`
    /// (from `build_draft_rfc822`), carried in the payload so the replay
    /// executor's Message-ID search fallback has it.
    /// Current status of a draft (`"draft"`, `"outbox"`, `"sent"`, ...), or
    /// `None` if the id no longer exists (discarded). Used by the replay
    /// executor as a pre-flight guard for a queued `server_draft_save` op:
    /// the 2s autosave debounce can queue a save and only then schedule
    /// replay, so a user who sends or discards in that window must not have
    /// the stale save APPENDed to the server afterwards (S36 review fix).
    pub fn draft_status(&self, draft_id: i64) -> Result<Option<String>, DbError> {
        let conn = self.lock_conn();
        Ok(conn
            .query_row(
                "SELECT status FROM drafts WHERE id = ?1",
                params![draft_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn queue_server_draft_save(
        &self,
        draft_id: i64,
        message_id: &str,
        raw: &[u8],
    ) -> Result<(), DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let row: Option<(String, String)> = tx
            .query_row(
                "SELECT account_id, status FROM drafts WHERE id = ?1",
                params![draft_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((account_id, status)) = row else {
            return Err(DbError(format!("unknown draft id: {draft_id}")));
        };
        if status != "draft" {
            return Ok(()); // already sent/discarded before this save fired
        }
        if !write_enabled_on(&tx, &account_id)? {
            return Err(DbError(EVALUATION_MODE_MSG.into()));
        }
        let existing: Option<String> = tx
            .query_row(
                "SELECT path FROM mailboxes WHERE account_id = ?1 AND role = 'drafts' LIMIT 1",
                params![account_id],
                |r| r.get(0),
            )
            .optional()?;
        let dest_path = match existing {
            Some(p) => p,
            None => {
                let path = default_path_for_role("drafts")?;
                let mailbox_id = format!("{account_id}:{path}");
                tx.execute(
                    "INSERT INTO mailboxes (id, account_id, path, name, role)
                     VALUES (?1, ?2, ?3, ?3, 'drafts')
                     ON CONFLICT(id) DO UPDATE SET role = excluded.role",
                    params![mailbox_id, account_id, path],
                )?;
                path
            }
        };
        let payload = serde_json::json!({
            "draft_id": draft_id,
            "dest_path": dest_path,
            "message_id": message_id,
        })
        .to_string();
        tx.execute(
            "INSERT INTO operations (account_id, kind, payload_json) VALUES (?1, 'server_draft_save', ?2)",
            params![account_id, payload],
        )?;
        let op_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO operation_blobs (operation_id, bytes) VALUES (?1, ?2)",
            params![op_id, raw],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Enqueue a self-contained `server_draft_cleanup` op: `delete_uid` at
    /// replay time, no local row to touch. Shared by
    /// `record_server_draft_saved` (replacing a superseded copy),
    /// `send_draft_delayed` and `delete_draft` (removing the copy on
    /// send/discard). Takes an open transaction so it composes atomically
    /// with the caller's own write.
    ///
    /// `uid_validity` is the UID space `uid` was recorded in (the draft row's
    /// `server_uid_validity`, or the value the APPEND just reported). It
    /// travels in the payload because the op is a permanent delete by UID:
    /// replay deletes only while the mailbox still reports that exact value
    ///. An unknown one (`None`, or non-positive) is stored
    /// as 0, which replay never matches: the server copy is then left behind
    /// rather than risk deleting whatever message holds that UID now.
    fn queue_server_draft_cleanup_tx(
        tx: &rusqlite::Transaction<'_>,
        account_id: &str,
        mailbox_path: &str,
        uid_validity: Option<i64>,
        uid: i64,
    ) -> Result<(), DbError> {
        let payload = serde_json::json!({
            "mailbox_path": mailbox_path,
            "uid": uid,
            "uid_validity": uid_validity.filter(|v| *v > 0).unwrap_or(0),
        })
        .to_string();
        tx.execute(
            "INSERT INTO operations (account_id, kind, payload_json) VALUES (?1, 'server_draft_cleanup', ?2)",
            params![account_id, payload],
        )?;
        Ok(())
    }

    /// Queue the removal of one server draft copy that no draft row tracks:
    /// a duplicate left by a retried `server_draft_save`.
    /// `uid_validity` is the UID space the replay executor just found `uid`
    /// in. Called only from inside a replay run, which evaluation mode
    /// already keeps from starting; the op itself is gated there again.
    pub fn queue_server_draft_cleanup(
        &self,
        account_id: &str,
        mailbox_path: &str,
        uid_validity: i64,
        uid: i64,
    ) -> Result<(), DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        Self::queue_server_draft_cleanup_tx(
            &tx,
            account_id,
            mailbox_path,
            Some(uid_validity),
            uid,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The account a draft currently belongs to and its status, or `None`
    /// when the draft no longer exists. The replay executor's pre-flight for
    /// a `server_draft_save` op: the save is mirrored only while the draft
    /// is still a `draft` AND still belongs to the account that queued the
    /// op.
    pub fn draft_account_and_status(
        &self,
        draft_id: i64,
    ) -> Result<Option<(String, String)>, DbError> {
        let conn = self.lock_conn();
        Ok(conn
            .query_row(
                "SELECT account_id, status FROM drafts WHERE id = ?1",
                params![draft_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// Called by the replay executor after a successful
    /// `append_with_flags_get_uid` (+ Message-ID search fallback if needed).
    /// Records the new server location on the draft and, if a DIFFERENT
    /// prior copy existed, enqueues its cleanup (append-new-then-
    /// expunge-old — the new copy is already safely on the server before
    /// any deletion is queued). `uid`/`message_id` may both be `None`/empty
    /// only in the (rare) case neither APPENDUID nor the search fallback
    /// resolved a UID — `server_message_id` is still recorded so the NEXT
    /// save can search for and clean up this copy. Returns `false` if the
    /// draft row is gone by the time this runs (the user deleted it
    /// mid-flight); the just-appended copy is then queued for cleanup, since
    /// `delete_draft`'s own cleanup only fires for a mirror it can see.
    ///
    /// S36 review fix: the replay executor checks `draft_status` before
    /// calling `append_with_flags_get_uid`, so this normally never sees a
    /// non-'draft' row — but that check and the network round-trip to the
    /// server are not atomic with a concurrent `send_draft_delayed`/
    /// `delete_draft`. If the status flipped away from 'draft' in that
    /// window, the APPEND already landed on the server: rather than record
    /// it as the live mirror on a row that no longer wants one, queue the
    /// just-appended copy (and any distinct prior mirror) for cleanup and
    /// leave the row's mirror fields untouched. Also returns `false`.
    ///
    ///: `op_account_id` is the account whose replay ran the
    /// APPEND, i.e. the account whose Drafts mailbox now holds the copy. It
    /// is NOT read from the draft row, because the user can switch the
    /// draft's From account while the save is in flight. When the row has
    /// moved to another account (or is gone), the copy is queued for cleanup
    /// under `op_account_id` and the row is left untouched: recording one
    /// account's path and UID on another account's draft would later delete
    /// whatever message holds that UID in the wrong account. Returns `false`.
    pub fn record_server_draft_saved(
        &self,
        draft_id: i64,
        op_account_id: &str,
        mailbox_path: &str,
        uid_validity: Option<i64>,
        uid: Option<i64>,
        message_id: &str,
    ) -> Result<bool, DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        #[allow(clippy::type_complexity)]
        let prior: Option<(String, String, Option<String>, Option<i64>, Option<i64>)> = tx
            .query_row(
                "SELECT account_id, status, server_mailbox_path, server_uid, server_uid_validity
                 FROM drafts WHERE id = ?1",
                params![draft_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let owned_by_op_account = prior
            .as_ref()
            .is_some_and(|(account_id, ..)| account_id == op_account_id);
        if !owned_by_op_account {
            // The draft was discarded, or its From account changed, during
            // the APPEND round-trip. The copy sits in `op_account_id`'s
            // Drafts and no row will ever track it: remove it there.
            if let Some(uid) = uid {
                Self::queue_server_draft_cleanup_tx(
                    &tx,
                    op_account_id,
                    mailbox_path,
                    uid_validity,
                    uid,
                )?;
            }
            tx.commit()?;
            return Ok(false);
        }
        let Some((account_id, status, prior_path, prior_uid, prior_uid_validity)) = prior else {
            return Ok(false);
        };
        if status != "draft" {
            // Superseded between the replay guard's check and this call
            // (send/discard raced the APPEND round-trip). Clean up the copy
            // this call just resolved, plus any distinct earlier mirror,
            // instead of recording either as live.
            if let Some(uid) = uid {
                Self::queue_server_draft_cleanup_tx(
                    &tx,
                    op_account_id,
                    mailbox_path,
                    uid_validity,
                    uid,
                )?;
            }
            if let (Some(prior_path), Some(prior_uid)) = (prior_path, prior_uid) {
                if prior_path != mailbox_path || Some(prior_uid) != uid {
                    Self::queue_server_draft_cleanup_tx(
                        &tx,
                        &account_id,
                        &prior_path,
                        prior_uid_validity,
                        prior_uid,
                    )?;
                }
            }
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            "UPDATE drafts
             SET server_mailbox_path = ?2, server_uid = ?3, server_uid_validity = ?4,
                 server_message_id = ?5, server_saved_at_epoch = ?6
             WHERE id = ?1",
            params![
                draft_id,
                mailbox_path,
                uid,
                uid_validity,
                message_id,
                now_epoch()
            ],
        )?;
        if let (Some(prior_path), Some(prior_uid)) = (prior_path, prior_uid) {
            let superseded = prior_path != mailbox_path || Some(prior_uid) != uid;
            if superseded {
                Self::queue_server_draft_cleanup_tx(
                    &tx,
                    &account_id,
                    &prior_path,
                    prior_uid_validity,
                    prior_uid,
                )?;
            }
        }
        tx.commit()?;
        Ok(true)
    }

    /// Given the local id of an already-synced message living in an
    /// account's Drafts-role mailbox (ordinary sync already pulls Drafts by
    /// default), create or reuse a local editable `drafts` row mirroring it.
    /// Idempotent: a second call for the same server message returns the
    /// same draft id rather than duplicating it. Errors if the message is
    /// unknown or its mailbox has no `role = 'drafts'`.
    ///
    /// This variant has no source for attachment bytes, so a message with
    /// attachments always hydrates as INCOMPLETE (see
    /// [`Self::open_server_draft_with`]); callers that can read the
    /// attachment cache use that one.
    pub fn open_server_draft(&self, message_id: &str) -> Result<i64, DbError> {
        self.open_server_draft_with(message_id, |_| None)
    }

    /// [`Self::open_server_draft`] with a byte source for cached attachments:
    /// `load_cached(content_hash)` returns the bytes of an `attachment_cache`
    /// entry, or `None` when they are not available locally. It is called
    /// while the DB lock is held, so it must be local I/O only.
    ///
    /// What is carried over (#3436): recipients, subject and text body; the
    /// From identity when it is one of the account's (the account's own
    /// address is the NULL default; a foreign From is never adopted); reply
    /// threading rebuilt from
    /// `message_references`; and every attachment whose bytes `load_cached`
    /// can supply, with its inline flag and Content-ID.
    ///
    /// COMPLETE vs INCOMPLETE. A tracked draft is destructive by design: the
    /// first autosave APPENDs our rebuild and `record_server_draft_saved`
    /// then expunges the tracked UID, and Send/discard expunge it too. That
    /// is only safe when the local row holds everything the server copy
    /// does. So the row tracks the server UID only when hydration was
    /// complete. Otherwise (an attachment's bytes are not cached, the body
    /// was never fetched, or it is HTML-only and would open blank in the
    /// plain-text composer) the row is created UNTRACKED: `server_uid` stays
    /// NULL, so no path ever queues a cleanup for the original, which stays
    /// in the server's Drafts folder beside whatever we save. Such a row is
    /// still found again through `server_mailbox_path` + `server_message_id`
    /// (until its first mirror save replaces them), which keeps this
    /// idempotent.
    pub fn open_server_draft_with<F>(
        &self,
        message_id: &str,
        mut load_cached: F,
    ) -> Result<i64, DbError>
    where
        F: FnMut(&str) -> Option<Vec<u8>>,
    {
        struct Source {
            account_id: String,
            path: String,
            provider_uid: Option<i64>,
            rfc822_message_id: Option<String>,
            subject: Option<String>,
            from_address: Option<String>,
            has_attachments: bool,
        }
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let msg: Option<Source> = tx
            .query_row(
                "SELECT m.account_id, mb.path, m.provider_uid, m.rfc822_message_id, m.subject,
                        m.from_address, m.has_attachments
                 FROM messages m JOIN mailboxes mb ON mb.id = m.mailbox_id
                 WHERE m.id = ?1 AND mb.role = 'drafts'",
                params![message_id],
                |r| {
                    Ok(Source {
                        account_id: r.get(0)?,
                        path: r.get(1)?,
                        provider_uid: r.get(2)?,
                        rfc822_message_id: r.get(3)?,
                        subject: r.get(4)?,
                        from_address: r.get(5)?,
                        has_attachments: r.get(6)?,
                    })
                },
            )
            .optional()?;
        let Some(msg) = msg else {
            return Err(DbError(format!(
                "unknown message id, or not in a Drafts-role mailbox: {message_id}"
            )));
        };
        let Some(uid) = msg.provider_uid else {
            return Err(DbError(format!("message {message_id} has no provider uid")));
        };

        // Scoped to the message's own account: UIDs are per mailbox, and two
        // accounts routinely share a Drafts path and low UIDs (#3444). The
        // second arm finds an untracked (incomplete) hydration again.
        let existing_draft: Option<i64> = tx
            .query_row(
                "SELECT id FROM drafts
                 WHERE account_id = ?1 AND status = 'draft' AND server_mailbox_path = ?2
                   AND (server_uid = ?3
                        OR (server_uid IS NULL AND ?4 IS NOT NULL AND server_message_id = ?4))
                 ORDER BY server_uid IS NULL, id LIMIT 1",
                params![msg.account_id, msg.path, uid, msg.rfc822_message_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing_draft {
            tx.commit()?;
            return Ok(id);
        }

        // (text, html); the outer None means the body was never fetched.
        let body: Option<(Option<String>, Option<String>)> = tx
            .query_row(
                "SELECT text_body, html_body FROM message_bodies WHERE message_id = ?1",
                params![message_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let body_fetched = body.is_some();
        let (text_body, html_body) = body.unwrap_or((None, None));
        let has_text = text_body.as_deref().is_some_and(|t| !t.trim().is_empty());
        let html_only = !has_text && html_body.as_deref().is_some_and(|h| !h.trim().is_empty());

        let mut stmt = tx.prepare(
            "SELECT kind, name, address FROM message_addresses
             WHERE message_id = ?1 ORDER BY ordinal",
        )?;
        let addrs: Vec<(String, Option<String>, String)> = stmt
            .query_map(params![message_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        let addr_json = |kind: &str| -> String {
            let filtered: Vec<DraftAddress> = addrs
                .iter()
                .filter(|(k, _, _)| k == kind)
                .map(|(_, name, address)| DraftAddress {
                    name: name.clone(),
                    email: address.clone(),
                })
                .collect();
            serde_json::to_string(&filtered).unwrap_or_else(|_| "[]".to_string())
        };

        // From: only one of this account's identities is adopted, written
        // exactly as the compose From picker would save it (the identity's
        // own spelling and display name; the picker matches on the stored
        // email). The account's own address is the NULL default, and a
        // foreign From is never adopted.
        let from_address = msg
            .from_address
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty());
        let identity: Option<(String, Option<String>)> = match from_address {
            Some(address) => tx
                .query_row(
                    "SELECT email, display_name FROM account_identities
                     WHERE account_id = ?1 AND lower(email) = lower(?2)
                     ORDER BY id LIMIT 1",
                    params![msg.account_id, address],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?,
            None => None,
        };
        let (from_email, from_name) = match identity {
            Some((email, display_name)) => (Some(email), display_name),
            None => (None, None),
        };

        // Threading. `message_references` holds In-Reply-To first, then the
        // rest of References, bracket-free and de-duplicated
        // (`castellum-mail`), so ordinal 0 is the parent and belongs LAST in
        // the rebuilt References header. (A draft that had References but no
        // In-Reply-To gets its first reference as In-Reply-To; the
        // distinction is not stored.)
        let mut stmt = tx.prepare(
            "SELECT ref_message_id FROM message_references
             WHERE message_id = ?1 ORDER BY ordinal",
        )?;
        let refs: Vec<String> = stmt
            .query_map(params![message_id], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        let bracket = |id: &String| format!("<{id}>");
        let in_reply_to = refs.first().map(bracket);
        let references_header = refs.first().map(|parent| {
            refs[1..]
                .iter()
                .chain(std::iter::once(parent))
                .map(bracket)
                .collect::<Vec<_>>()
                .join(" ")
        });

        // Attachments: read every available byte source BEFORE inserting, so
        // completeness is known when the tracking decision is made.
        struct Carried {
            filename: String,
            content_type: String,
            is_inline: bool,
            content_id: Option<String>,
            bytes: Vec<u8>,
        }
        let mut stmt = tx.prepare(
            "SELECT ma.filename, ma.content_type, ma.is_inline, ma.content_id, ac.content_hash
             FROM message_attachments ma
             LEFT JOIN attachment_cache ac
                    ON ac.message_id = ma.message_id AND ac.ordinal = ma.ordinal
             WHERE ma.message_id = ?1 ORDER BY ma.ordinal",
        )?;
        #[allow(clippy::type_complexity)]
        let descriptors: Vec<(
            Option<String>,
            Option<String>,
            bool,
            Option<String>,
            Option<String>,
        )> = stmt
            .query_map(params![message_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        let descriptor_count = descriptors.len();
        let mut carried: Vec<Carried> = Vec::with_capacity(descriptor_count);
        for (filename, content_type, is_inline, content_id, content_hash) in descriptors {
            let Some(bytes) = content_hash.as_deref().and_then(&mut load_cached) else {
                continue;
            };
            carried.push(Carried {
                filename: filename.unwrap_or_else(|| "attachment".to_string()),
                content_type: content_type
                    .unwrap_or_else(|| "application/octet-stream".to_string()),
                is_inline,
                content_id,
                bytes,
            });
        }
        let attachments_complete =
            carried.len() == descriptor_count && !(msg.has_attachments && descriptor_count == 0);
        let complete = body_fetched && !html_only && attachments_complete;
        let tracked_uid = complete.then_some(uid);

        // The UID comes from the synced row, so it belongs to the UID space
        // the Drafts sync cursor records. Stored with the mirror because the
        // copy's later cleanup is a delete by UID that replay refuses without
        // a known UIDVALIDITY.
        let uid_validity = mailbox_uid_validity_on(&tx, &msg.account_id, &msg.path)?;
        tx.execute(
            "INSERT INTO drafts
                 (account_id, to_json, cc_json, bcc_json, subject, body_text,
                  from_email, from_name, in_reply_to, references_header,
                  server_mailbox_path, server_uid, server_message_id, server_saved_at_epoch,
                  server_uid_validity)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                msg.account_id,
                addr_json("to"),
                addr_json("cc"),
                addr_json("bcc"),
                msg.subject.unwrap_or_default(),
                text_body.unwrap_or_default(),
                from_email,
                from_name,
                in_reply_to,
                references_header,
                msg.path,
                tracked_uid,
                msg.rfc822_message_id,
                now_epoch(),
                uid_validity,
            ],
        )?;
        let id = tx.last_insert_rowid();
        for (ordinal, a) in carried.iter().enumerate() {
            tx.execute(
                "INSERT INTO draft_attachments
                     (draft_id, ordinal, filename, content_type, size_bytes, is_inline, content_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    id,
                    ordinal as i64,
                    a.filename,
                    a.content_type,
                    a.bytes.len() as i64,
                    a.is_inline,
                    a.content_id
                ],
            )?;
            tx.execute(
                "INSERT INTO draft_attachment_blobs (attachment_id, bytes) VALUES (?1, ?2)",
                params![tx.last_insert_rowid(), a.bytes],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// Called once per account after an ordinary sync ingests the Drafts
    /// mailbox (`castellum-core::sync` wires this in). For every local draft
    /// still tracking a server copy: if that exact UID is STILL present in
    /// `messages` for the tracked mailbox, there is no conflict (nothing to
    /// do — including the ordinary case where WE are mid-edit and haven't
    /// saved yet, since our own replace-on-edit hasn't run). If it has
    /// VANISHED (no longer in `messages` — expunged, whether by another
    /// client's replace or our own not-yet-locally-reflected cleanup) AND a
    /// DIFFERENT message now exists in that same mailbox with a NEWER
    /// `sent_at` (INTERNALDATE) than our local `updated_at_epoch`, another
    /// client's edit is presumed newer: drop this draft's server pointer and
    /// keep the SAME row (same id, attachments, From, HTML) as a
    /// server-untracked draft labelled " (kept locally)", so no in-progress
    /// local edit is lost and an open compose window keeps saving to it.
    /// If no newer candidate exists, the vanished UID is treated as our
    /// own (not-yet-locally-reflected) cleanup and the local draft's server
    /// pointer is simply cleared — the next autosave will re-append fresh,
    /// this time correctly recognising there's no live prior copy to expunge.
    /// Read-only w.r.t. the server: this never talks to the provider, only
    /// the DB. Returns the number of drafts where the server pointer changed
    /// (adopted or cleared) — informational, used by tests.
    pub fn reconcile_server_draft_conflicts(&self, account_id: &str) -> Result<usize, DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        struct Tracked {
            id: i64,
            path: String,
            uid: i64,
            updated_at_epoch: i64,
        }
        let mut stmt = tx.prepare(
            "SELECT id, server_mailbox_path, server_uid,
                    CAST(strftime('%s', updated_at) AS INTEGER)
             FROM drafts
             WHERE account_id = ?1 AND status = 'draft'
               AND server_mailbox_path IS NOT NULL AND server_uid IS NOT NULL",
        )?;
        let tracked: Vec<Tracked> = stmt
            .query_map(params![account_id], |r| {
                Ok(Tracked {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    uid: r.get(2)?,
                    updated_at_epoch: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        let mut changed = 0usize;
        for t in tracked {
            let still_present: bool = tx
                .query_row(
                    "SELECT 1 FROM messages m JOIN mailboxes mb ON mb.id = m.mailbox_id
                     WHERE mb.account_id = ?1 AND mb.path = ?2 AND m.provider_uid = ?3",
                    params![account_id, t.path, t.uid],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if still_present {
                continue;
            }
            // A message another local draft already mirrors is THAT draft's
            // copy, never a newer edit of this one.
            let newer: Option<String> = tx
                .query_row(
                    "SELECT m.id
                     FROM messages m JOIN mailboxes mb2 ON mb2.id = m.mailbox_id
                     WHERE mb2.account_id = ?1 AND mb2.path = ?2
                       AND COALESCE(m.sent_at, 0) > ?3
                       AND NOT EXISTS (
                           SELECT 1 FROM drafts d2
                           WHERE d2.account_id = ?1 AND d2.status = 'draft'
                             AND d2.id <> ?4
                             AND d2.server_mailbox_path = ?2
                             AND d2.server_uid = m.provider_uid)
                     ORDER BY m.sent_at DESC LIMIT 1",
                    params![account_id, t.path, t.updated_at_epoch, t.id],
                    |r| r.get(0),
                )
                .optional()?;
            // Either way the row is only ever UPDATEd in place (#3437). It
            // used to be replaced by a copy and DELETEd in the conflict arm,
            // which cascaded to `draft_attachments`, dropped From and the
            // Markdown HTML, and invalidated the id an open compose window
            // was still saving to ("unknown draft id" on its next autosave).
            // The candidate is "any later message in the folder", not proof
            // of a real conflict, so nothing destructive may hang off it.
            // The newer server copy is opened explicitly by the user via
            // `open_server_draft`, not auto-adopted here.
            let subject_sql = if newer.is_some() {
                "CASE WHEN subject LIKE '% (kept locally)' THEN subject
                      ELSE subject || ' (kept locally)' END"
            } else {
                "subject"
            };
            tx.execute(
                &format!(
                    "UPDATE drafts SET server_mailbox_path = NULL, server_uid = NULL,
                         server_uid_validity = NULL, server_message_id = NULL,
                         subject = {subject_sql}
                     WHERE id = ?1"
                ),
                params![t.id],
            )?;
            changed += 1;
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Fetch the large payload bytes for one operation, if any. Executors call
    /// this at apply time so listing pending operations never loads blobs.
    pub fn operation_blob(&self, operation_id: i64) -> Result<Option<Vec<u8>>, DbError> {
        let conn = self.lock_conn();
        Ok(conn
            .query_row(
                "SELECT bytes FROM operation_blobs WHERE operation_id = ?1",
                params![operation_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Queue an `.eml` import as a durable `import_eml` operation + blob, in
    /// one transaction — the exact same shape `mark_draft_sent_with_copy`
    /// uses for `sent_copy`: the operation carries only routing fields
    /// (destination path, the Message-ID used for the provider's duplicate
    /// guard); the raw bytes go to `operation_blobs` keyed by the op id, so
    /// listing pending operations stays cheap and the import survives a
    /// crash before replay. No local message row is created here — the
    /// imported copy becomes visible once the account's next sync discovers
    /// the appended UID, mirroring how a sent copy only appears in Sent
    /// after sync catches it.
    ///
    /// Errors (not `Ok` with a blocked/failed status) when `mailbox_id`
    /// does not exist or does not belong to `account_id` — this is a
    /// caller-side bug (the Swift layer only ever offers mailboxes it
    /// already loaded for the account it's importing into), not a
    /// user-facing outcome to surface gracefully.
    pub fn enqueue_eml_import(
        &self,
        account_id: &str,
        mailbox_id: &str,
        message_id_header: &str,
        raw: &[u8],
    ) -> Result<i64, DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let dest_path: Option<String> = tx
            .query_row(
                "SELECT path FROM mailboxes WHERE id = ?1 AND account_id = ?2",
                params![mailbox_id, account_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(dest_path) = dest_path else {
            return Err(DbError(format!(
                "mailbox {mailbox_id} does not belong to account {account_id}"
            )));
        };
        let payload = serde_json::json!({
            "dest_path": dest_path,
            "message_id": message_id_header,
        })
        .to_string();
        tx.execute(
            "INSERT INTO operations (account_id, kind, payload_json) VALUES (?1, 'import_eml', ?2)",
            params![account_id, payload],
        )?;
        let op_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO operation_blobs (operation_id, bytes) VALUES (?1, ?2)",
            params![op_id, raw],
        )?;
        tx.commit()?;
        Ok(op_id)
    }
}

/// Columns selected by every draft read, in `row_to_draft` order.
const DRAFT_COLUMNS: &str = "id, account_id, to_json, cc_json, bcc_json, subject, body_text,
     body_html, from_email, from_name, in_reply_to, references_header, status,
     CAST(strftime('%s', updated_at) AS INTEGER),
     server_mailbox_path, server_uid, server_uid_validity, server_message_id,
     server_saved_at_epoch";

/// A raw drafts row (address columns still JSON strings).
struct DraftRow {
    id: i64,
    account_id: String,
    to_json: String,
    cc_json: String,
    bcc_json: String,
    subject: String,
    body_text: String,
    body_html: Option<String>,
    from_email: Option<String>,
    from_name: Option<String>,
    in_reply_to: Option<String>,
    references_header: Option<String>,
    status: String,
    updated_at_epoch: i64,
    server_mailbox_path: Option<String>,
    server_uid: Option<i64>,
    server_uid_validity: Option<i64>,
    server_message_id: Option<String>,
    server_saved_at_epoch: Option<i64>,
}

fn row_to_draft(r: &rusqlite::Row<'_>) -> rusqlite::Result<DraftRow> {
    Ok(DraftRow {
        id: r.get(0)?,
        account_id: r.get(1)?,
        to_json: r.get(2)?,
        cc_json: r.get(3)?,
        bcc_json: r.get(4)?,
        subject: r.get(5)?,
        body_text: r.get(6)?,
        body_html: r.get(7)?,
        from_email: r.get(8)?,
        from_name: r.get(9)?,
        in_reply_to: r.get(10)?,
        references_header: r.get(11)?,
        status: r.get(12)?,
        updated_at_epoch: r.get(13)?,
        server_mailbox_path: r.get(14)?,
        server_uid: r.get(15)?,
        server_uid_validity: r.get(16)?,
        server_message_id: r.get(17)?,
        server_saved_at_epoch: r.get(18)?,
    })
}

fn draft_from_row(row: DraftRow) -> Result<Draft, DbError> {
    Ok(Draft {
        id: row.id,
        account_id: row.account_id,
        to: addrs_from_json(&row.to_json)?,
        cc: addrs_from_json(&row.cc_json)?,
        bcc: addrs_from_json(&row.bcc_json)?,
        subject: row.subject,
        body_text: row.body_text,
        body_html: row.body_html,
        from_email: row.from_email,
        from_name: row.from_name,
        in_reply_to: row.in_reply_to,
        references_header: row.references_header,
        status: row.status,
        updated_at_epoch: row.updated_at_epoch,
        server_mailbox_path: row.server_mailbox_path,
        server_uid: row.server_uid,
        server_uid_validity: row.server_uid_validity,
        server_message_id: row.server_message_id,
        server_saved_at_epoch: row.server_saved_at_epoch,
    })
}

fn addrs_to_json(addrs: &[DraftAddress]) -> Result<String, DbError> {
    serde_json::to_string(addrs).map_err(|e| DbError(format!("serialize addresses: {e}")))
}

fn addrs_from_json(json: &str) -> Result<Vec<DraftAddress>, DbError> {
    serde_json::from_str(json).map_err(|e| DbError(format!("bad stored addresses: {e}")))
}

/// `Database::mailbox_uid_validity` on an open connection or transaction.
fn mailbox_uid_validity_on(
    conn: &rusqlite::Connection,
    account_id: &str,
    path: &str,
) -> Result<Option<i64>, DbError> {
    let value: Option<i64> = conn
        .query_row(
            "SELECT c.uid_validity
             FROM sync_cursors c JOIN mailboxes mb ON mb.id = c.mailbox_id
             WHERE c.account_id = ?1 AND mb.account_id = ?1 AND mb.path = ?2",
            params![account_id, path],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten();
    Ok(value.filter(|v| *v > 0))
}

/// Error unless the draft exists and is still editable (status `draft`).
fn require_draft_status(
    tx: &rusqlite::Transaction<'_>,
    id: i64,
    action: &str,
) -> Result<(), DbError> {
    let status: Option<String> = tx
        .query_row(
            "SELECT status FROM drafts WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    match status.as_deref() {
        None => Err(DbError(format!("unknown draft id: {id}"))),
        Some("draft") => Ok(()),
        Some(other) => Err(DbError(format!(
            "cannot {action} draft {id}: status is '{other}', not 'draft'"
        ))),
    }
}

/// Compact a sorted-ascending list of provider UIDs into IMAP sequence-set
/// syntax (`seq-number *("," seq-number / seq-range)`, RFC 3501), e.g.
/// `[3, 5, 6, 7, 10]` -> `"3,5:7,10"`. A run of 2+ consecutive UIDs becomes a
/// `first:last` range; everything else is listed individually. Empty input
/// yields `""` — callers must treat that as "nothing to expunge" and never
/// pass it to `SequenceSet::try_from` (which rejects an empty set). Used only
/// by `Database::empty_trash` to keep its snapshot payload small even for a
/// large Trash, per the operation-payload-size decision in DECISIONS.md.
fn compact_uid_ranges(uids: &[i64]) -> String {
    let mut parts = Vec::new();
    let mut i = 0;
    while i < uids.len() {
        let start = uids[i];
        let mut end = start;
        while i + 1 < uids.len() && uids[i + 1] == end + 1 {
            end = uids[i + 1];
            i += 1;
        }
        if start == end {
            parts.push(start.to_string());
        } else {
            parts.push(format!("{start}:{end}"));
        }
        i += 1;
    }
    parts.join(",")
}

/// Default folder path to create for a special-use role when the account has
/// none. Only roles we write into (archive/trash moves, sent copies) have a
/// sensible default.
fn default_path_for_role(role: &str) -> Result<String, DbError> {
    match role {
        "trash" => Ok("Trash".to_string()),
        "archive" => Ok("Archive".to_string()),
        "sent" => Ok("Sent".to_string()),
        "drafts" => Ok("Drafts".to_string()),
        _ => Err(DbError(format!("no {role} mailbox and no default path"))),
    }
}

/// Enqueue a self-contained `move` op (src path/uid + Message-ID + dest path)
/// and remove the local message row. Shared by archive and delete. The
/// payload carries everything the executor needs, so the row can go
/// immediately.
///
/// `uid_validity` is the SOURCE mailbox's cursor value AT THIS MOMENT (`0` if
/// no cursor exists yet — e.g. an interrupted first import that never wrote
/// one). It is recorded now, not re-read later: if `Database::empty_trash`
/// later rewrites this op into a `delete_permanent` (the message hasn't
/// reached Trash yet — see `rewrite_pending_moves_to_trash_as_permanent_deletes`),
/// it must stamp the UID space the move actually SAW, never whatever the
/// cursor happens to say by the time Empty Trash runs — a later sync could
/// have advanced it past a UIDVALIDITY reset the first-import path never
/// detects (no prior cursor to compare against), which would otherwise let
/// a stale UID be expunged in a UID space it never belonged to (round-2
/// PR #165 review).
///
/// Returns the new operation's id, or `None` for the "already in the
/// destination" no-op (nothing queued, row kept). The id is the handle
/// [`Database::undo_moves`] takes.
fn enqueue_move(
    tx: &rusqlite::Transaction<'_>,
    message_id: &str,
    dest_path: &str,
) -> Result<Option<i64>, DbError> {
    let row = tx
        .query_row(
            "SELECT m.account_id, mb.id, mb.path, m.provider_uid, m.rfc822_message_id,
                    m.sent_at, m.from_address, m.subject, m.size_bytes
             FROM messages m JOIN mailboxes mb ON mb.id = m.mailbox_id
             WHERE m.id = ?1",
            params![message_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    crate::rules::anonymous_message_fingerprint(
                        r.get::<_, Option<i64>>(5)?,
                        r.get::<_, Option<String>>(6)?.as_deref(),
                        r.get::<_, Option<String>>(7)?.as_deref(),
                        r.get::<_, Option<i64>>(8)?,
                    ),
                ))
            },
        )
        .optional()?;
    let Some((account_id, src_mailbox_id, src_path, uid, rfc, fingerprint)) = row else {
        return Err(DbError(format!("unknown message id: {message_id}")));
    };
    // Already in the destination: queue nothing and keep the row. Archive of a message
    // that is already in Archive (reachable from search / smart-mailbox lists) used to
    // queue a MOVE onto its own folder and drop the local row until the next sync
    // (#3459). A no-op rather than an error, so a mixed multi-selection does not fail.
    if src_path == dest_path {
        return Ok(None);
    }
    let Some(uid) = uid else {
        return Err(DbError(format!("message {message_id} has no provider uid")));
    };
    let uid_validity: i64 = tx
        .query_row(
            "SELECT uid_validity FROM sync_cursors WHERE account_id = ?1 AND mailbox_id = ?2",
            params![account_id, src_mailbox_id],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten()
        .unwrap_or(0);
    // Mac-only tags (origin='local': demoted by PERMANENTFLAGS, or added in
    // evaluation mode) exist only in `message_keywords`, which cascades away
    // with the row deleted below. Carry them in the op payload; the arrival
    // in the destination re-attaches them (`reattach_local_keywords_after_move`).
    let local_keywords: Vec<(String, String)> = {
        let mut stmt = tx.prepare(
            "SELECT keyword_key, keyword FROM message_keywords
             WHERE message_id = ?1 AND origin = 'local' ORDER BY keyword_key",
        )?;
        let rows = stmt.query_map(params![message_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    let payload = serde_json::json!({
        "src_path": src_path,
        "uid": uid,
        "message_id": rfc.unwrap_or_default(),
        "dest_path": dest_path,
        "uid_validity": uid_validity,
        "local_keywords": local_keywords,
        // Identifies a message that has no Message-ID (see
        // `arrivals_following_own_moves`); a hash, never the headers.
        "fingerprint": fingerprint,
    })
    .to_string();
    tx.execute(
        "INSERT INTO operations (account_id, kind, payload_json) VALUES (?1, 'move', ?2)",
        params![account_id, payload],
    )?;
    let op_id = tx.last_insert_rowid();
    // The message is about to leave this account's message set entirely:
    // seed its thread's surviving members BEFORE the delete below removes
    // the only row a later closure walk could have found that thread
    // through, and drop the thread row after it if nobody is left (the tracker
    // #2504; `prepare_message_departures`).
    let departing = [message_id.to_string()];
    let threads = prepare_message_departures(tx, &account_id, &departing)?;
    // Read state, flag and tag ops still waiting for this message resolve
    // their IMAP coordinates from the row deleted below. Stamp the source
    // coordinates into them now, so they still reach the server: they replay
    // (in id order) before the move above, which carries the flags along.
    // The UIDVALIDITY that UID belongs to travels with it (0 = no cursor
    // yet), so replay can refuse the store if the mailbox was reset
    // meanwhile.
    tx.execute(
        "UPDATE operations
         SET payload_json = json_set(payload_json, '$.src_path', ?2, '$.uid', ?3,
                                     '$.uid_validity', ?5)
         WHERE account_id = ?1
           AND kind IN ('mark_read', 'flag', 'keyword')
           AND status NOT IN ('succeeded', 'cancelled')
           AND json_extract(payload_json, '$.message_id') = ?4",
        params![account_id, src_path, uid, message_id, uid_validity],
    )?;
    purge_ai_for_messages(tx, "SELECT ?1", params![message_id])?;
    tx.execute("DELETE FROM messages WHERE id = ?1", params![message_id])?;
    sweep_emptied_threads(tx, &account_id, &threads)?;
    purge_orphan_thread_ai_outputs(tx)?;
    Ok(Some(op_id))
}

/// Content hashes of downloaded attachments that ONLY `account_id` references
/// (no other account's `attachment_cache` row names them). Must be read BEFORE
/// the account's messages cascade away; the core layer unlinks the files
/// after commit.
fn orphaned_attachment_hashes(
    tx: &rusqlite::Transaction<'_>,
    account_id: &str,
) -> Result<Vec<String>, DbError> {
    let mut stmt = tx.prepare(
        "SELECT DISTINCT ac.content_hash FROM attachment_cache ac
           JOIN messages m ON m.id = ac.message_id
          WHERE m.account_id = ?1
            AND NOT EXISTS (
                SELECT 1 FROM attachment_cache ac2
                  JOIN messages m2 ON m2.id = ac2.message_id
                 WHERE ac2.content_hash = ac.content_hash
                   AND m2.account_id <> ?1)",
    )?;
    let rows = stmt.query_map(params![account_id], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// One outstanding Mac-only tag stash: a `move` op payload whose
/// `local_keywords` is non-empty (see `enqueue_move`).
struct MoveStash {
    op_id: i64,
    account_id: String,
    /// Folder the message is expected to (re)appear in: the move's
    /// destination, or -- when the move was cancelled or failed permanently,
    /// so the message never left -- its source.
    arrival_path: String,
    message_id: String,
    fingerprint: String,
    pairs: Vec<(String, String)>,
}

/// Load every outstanding stash once. Newest op first, so the first match
/// is the newest (the old per-message `ORDER BY id DESC LIMIT 1`). Empty in
/// the overwhelmingly common case, which makes the per-message cost zero.
fn load_move_stashes(tx: &rusqlite::Transaction<'_>) -> Result<Vec<MoveStash>, DbError> {
    let mut stmt = tx.prepare_cached(
        "SELECT id, account_id, status,
                json_extract(payload_json, '$.dest_path'),
                json_extract(payload_json, '$.src_path'),
                COALESCE(json_extract(payload_json, '$.message_id'), ''),
                COALESCE(json_extract(payload_json, '$.fingerprint'), ''),
                json_extract(payload_json, '$.local_keywords')
         FROM operations
         WHERE kind = 'move'
           AND created_at >= datetime('now', ?1)
           AND json_array_length(payload_json, '$.local_keywords') > 0
         ORDER BY id DESC",
    )?;
    let rows = stmt.query_map(params![crate::rules::OWN_MOVE_LOOKBACK], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, String>(7)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (op_id, account_id, status, dest, src, message_id, fingerprint, json) = row?;
        // A move that never happened leaves the message in its source
        // folder: the re-import there is the arrival to match.
        let never_moved = status == "cancelled" || status == "failed_permanent";
        let Some(arrival_path) = (if never_moved { src } else { dest }) else {
            continue;
        };
        out.push(MoveStash {
            op_id,
            account_id,
            arrival_path,
            message_id,
            fingerprint,
            pairs: serde_json::from_str(&json).unwrap_or_default(),
        });
    }
    Ok(out)
}

/// A message that arrives where one of our own `move` ops put it (or, for a
/// cancelled / permanently failed move, where it stayed) gets back the
/// Mac-only tags `enqueue_move` stashed in that op's payload (the source
/// row, and its `message_keywords`, were deleted at enqueue). Matched like
/// `arrivals_following_own_moves`: Message-ID, or the fingerprint for a
/// message without one, plus the folder. Consumed once: the stash is
/// removed from `stashes` and emptied in the op so a later duplicate does
/// not inherit it.
fn reattach_local_keywords_after_move(
    tx: &rusqlite::Transaction<'_>,
    m: &NewMessage,
    stashes: &mut Vec<MoveStash>,
) -> Result<(), DbError> {
    if !stashes.iter().any(|s| s.account_id == m.account_id) {
        return Ok(());
    }
    let rfc = m.rfc822_message_id.clone().unwrap_or_default();
    let fingerprint = crate::rules::anonymous_message_fingerprint(
        m.sent_at,
        m.from_address.as_deref(),
        m.subject.as_deref(),
        m.size_bytes,
    );
    let path: Option<String> = tx
        .prepare_cached("SELECT path FROM mailboxes WHERE id = ?1")?
        .query_row(params![m.mailbox_id], |r| r.get(0))
        .optional()?;
    let Some(path) = path else {
        return Ok(());
    };
    let Some(pos) = stashes.iter().position(|s| {
        s.account_id == m.account_id
            && s.arrival_path == path
            && if rfc.is_empty() {
                s.message_id.is_empty() && s.fingerprint == fingerprint
            } else {
                s.message_id == rfc
            }
    }) else {
        return Ok(());
    };
    let stash = stashes.remove(pos);
    for (key, keyword) in &stash.pairs {
        tx.execute(
            "INSERT OR IGNORE INTO message_keywords (message_id, keyword_key, keyword, origin)
             VALUES (?1, ?2, ?3, 'local')",
            params![m.id, key, keyword],
        )?;
    }
    tx.execute(
        "UPDATE operations SET payload_json = json_set(payload_json, '$.local_keywords', json('[]'))
         WHERE id = ?1",
        params![stash.op_id],
    )?;
    Ok(())
}

/// Rewrite every pending `move` op targeting `trash_path` into a
/// `delete_permanent` op on its SOURCE message instead (`Database::empty_trash`,
/// PR #165 review point 1). `enqueue_move` deletes the local row immediately
/// but the message only actually reaches server-side Trash once its `move`
/// op replays — until then it is invisible to `empty_trash`'s own snapshot of
/// Trash's local rows, so without this rewrite it would survive Empty Trash
/// on the server and reappear locally on the next sync.
///
/// "Pending" is `queued`/`failed`/`failed_permanent` — every non-terminal,
/// non-`succeeded` state EXCEPT `running`. A `running` op may be mid-flight
/// on the wire right now; mutating its row here could race the executor's
/// own status update, so it is left alone. Documented residual: if that
/// in-flight move completes AFTER this call, the moved message lands in
/// Trash post-snapshot and survives this Empty Trash click — the same class
/// of race the base snapshot design already accepts, and recoverable by
/// emptying Trash again.
///
/// The rewritten op resets to `queued`/0 attempts/no error: it is a distinct
/// action (permanent delete, not a move) with its own failure surface, so an
/// exhausted move's attempt count carries no useful information forward.
fn rewrite_pending_moves_to_trash_as_permanent_deletes(
    tx: &rusqlite::Transaction<'_>,
    account_id: &str,
    trash_path: &str,
) -> Result<(), DbError> {
    let mut stmt = tx.prepare(
        "SELECT id, payload_json FROM operations
         WHERE account_id = ?1 AND kind = 'move'
           AND status IN ('queued', 'failed', 'failed_permanent')
           AND json_extract(payload_json, '$.dest_path') = ?2
           -- A move the user has undone must not be turned into a permanent
           -- delete: the undo chained behind it brings the message back.
           AND COALESCE(json_extract(payload_json, '$.undone'), 0) = 0",
    )?;
    let rows: Vec<(i64, String)> = stmt
        .query_map(params![account_id, trash_path], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<Result<_, _>>()?;
    drop(stmt);
    for (op_id, payload_json) in rows {
        let payload: serde_json::Value = serde_json::from_str(&payload_json)
            .map_err(|e| DbError(format!("bad move payload on op {op_id}: {e}")))?;
        let src_path = payload["src_path"]
            .as_str()
            .ok_or_else(|| DbError(format!("move op {op_id} payload missing src_path")))?;
        let uid = payload["uid"]
            .as_i64()
            .ok_or_else(|| DbError(format!("move op {op_id} payload missing uid")))?;
        // The UIDVALIDITY the move ITSELF saw at enqueue time (`enqueue_move`),
        // NEVER re-read fresh here — the source mailbox's cursor may have
        // advanced past a reset since this move was queued (round-2 review).
        // A legacy move payload from before this field existed has no
        // `uid_validity` key at all: `unwrap_or(0)` gives it the same
        // fail-closed sentinel as "no cursor existed", since either way
        // there is no trustworthy snapshot to stand on.
        let uid_validity = payload["uid_validity"].as_i64().unwrap_or(0);

        let new_payload = serde_json::json!({
            "path": src_path,
            "uid_validity": uid_validity,
            "uids": uid.to_string(),
        })
        .to_string();
        tx.execute(
            "UPDATE operations
             SET kind = 'delete_permanent', payload_json = ?2,
                 status = 'queued', attempts = 0, last_error = NULL,
                 updated_at = datetime('now')
             WHERE id = ?1",
            params![op_id, new_payload],
        )?;
    }
    Ok(())
}

/// One message row used while rebuilding the thread projection.
struct ThreadRow {
    id: String,
    rfc: Option<String>,
    sent_at: Option<i64>,
    subject: Option<String>,
    is_read: bool,
    /// The thread the message points at right now, so a rebuild re-points
    /// only messages whose thread actually changed.
    thread_id: Option<String>,
}

impl ThreadRow {
    /// S42: this member's mute anchor -- `rfc822_message_id`, or `id:<id>`
    /// when the message has none (the same `COALESCE(rfc822_message_id,
    /// 'id:' || id)` `Database::mute_thread` writes). Matched against
    /// `thread_mutes.anchor`.
    fn mute_anchor(&self) -> std::borrow::Cow<'_, str> {
        match &self.rfc {
            Some(rfc) => std::borrow::Cow::Borrowed(rfc.as_str()),
            None => std::borrow::Cow::Owned(format!("id:{}", self.id)),
        }
    }
}

/// Which path [`Database::rebuild_threads_incremental`] took -- for its
/// debug log, the perf gate, and the tests that assert the incremental
/// path (not a fallback) is what actually ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadRebuildPath {
    /// Nothing was dirty and no full rebuild was forced.
    Noop,
    /// Only the dirty messages' connected groups were recomputed.
    Incremental {
        /// Dirty rows in the snapshot the pass started from.
        seeds: usize,
        /// Messages recomputed across every chunk.
        closure: usize,
        /// Chunk transactions written.
        chunks: usize,
    },
    /// The whole account was rebuilt instead.
    Full(FullThreadRebuildReason),
}

/// Why [`Database::rebuild_threads_incremental`] fell back to a full
/// rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullThreadRebuildReason {
    /// `thread_rebuild_state.force_full` was set (a mailbox purge, a
    /// UIDVALIDITY reset, or a pre-0039 dirty marker carried over).
    ForceFull,
    /// The account had no thread rows yet.
    FirstRun,
    /// More than `INCREMENTAL_DIRTY_FRACTION_LIMIT` of the account was dirty.
    DirtyFraction,
    /// One closure walk passed `INCREMENTAL_CLOSURE_LIMIT` messages.
    ClosureLimit,
}

/// Union-find `find` with path halving.
fn uf_find(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]];
        x = parent[x];
    }
    x
}

/// Union-find `union`.
fn uf_union(parent: &mut [usize], a: usize, b: usize) {
    let ra = uf_find(parent, a);
    let rb = uf_find(parent, b);
    if ra != rb {
        parent[ra] = rb;
    }
}

/// A computed thread group, ready to write: [`compute_thread_groups`]'s
/// output, [`write_thread_group`]'s input. Shared by [`Database::
/// rebuild_threads`] (full) and [`Database::rebuild_threads_incremental`]
/// so both write the SAME shape through the SAME function.
struct ThreadGroupWrite {
    thread_id: String,
    subject: Option<String>,
    first_message_at: Option<i64>,
    last_message_at: Option<i64>,
    message_count: i64,
    unread_count: i64,
    /// S42: any member's mute anchor is in `thread_mutes` -- recomputed on
    /// every write of the group (full AND incremental), so a mute survives
    /// a rebuild and follows a root-changing merge onto the new thread id.
    is_muted: bool,
    /// Every member. [`repoint_thread_extractions`] reads this to find which
    /// new group now holds a departing old thread's messages (the tracker
    /// #2637); the tests' oracle (`expected_projection`) reads it to state
    /// the full membership.
    member_ids: Vec<String>,
    /// The members whose current `thread_id` is not `thread_id` -- the only
    /// ones [`write_thread_group`] has to re-point.
    repoint_ids: Vec<String>,
}

/// The union-find grouping [`Database::rebuild_threads`] and
/// [`Database::rebuild_threads_incremental`] both use: rfc822 duplicates
/// join first, then references union each message with the first message
/// carrying the rfc822 id it cites (if we have one) -- byte-identical to
/// the algorithm this replaced, just factored out so there is exactly one
/// copy of it ('s whole correctness argument for the
/// incremental path is "same algorithm, smaller input"). `msgs`/`refs` may
/// be the whole account (full rebuild) or a proven-closed subset (an
/// incremental closure) -- either way, every reference among `msgs` must
/// resolve within `msgs`, or the caller's input wasn't actually closed.
///
/// `muted_anchors` is the account's `thread_mutes` anchors (S42), loaded
/// once by the caller ([`load_account_muted_anchors`]) -- never a per-group
/// query.
fn compute_thread_groups(
    msgs: &[ThreadRow],
    refs: &[(String, String)],
    muted_anchors: &HashSet<String>,
) -> Vec<ThreadGroupWrite> {
    let mut idx: HashMap<&str, usize> = HashMap::new();
    for (i, m) in msgs.iter().enumerate() {
        idx.insert(m.id.as_str(), i);
    }
    let mut parent: Vec<usize> = (0..msgs.len()).collect();

    // Map each rfc822 message-id to its first message, and union any
    // duplicates (the same id delivered to multiple mailboxes, e.g. Inbox +
    // Sent copies) so they land in one thread.
    let mut by_rfc: HashMap<&str, usize> = HashMap::new();
    for (i, m) in msgs.iter().enumerate() {
        if let Some(r) = &m.rfc {
            match by_rfc.get(r.as_str()) {
                Some(&first) => uf_union(&mut parent, first, i),
                None => {
                    by_rfc.insert(r.as_str(), i);
                }
            }
        }
    }

    // Union each message with every referenced message we actually have, and
    // with every other message citing the same id: replies to a message that
    // is not in the local mailbox (unsynced folder, skipped All Mail, a list
    // that predates the subscription) still share an ancestor, and must not
    // fall apart until that ancestor happens to arrive (#3458). The closure
    // walk in `expand_thread_closure` follows the same edge.
    let mut first_citer: HashMap<&str, usize> = HashMap::new();
    for (mid, refid) in refs {
        let Some(&a) = idx.get(mid.as_str()) else {
            continue;
        };
        if let Some(&b) = by_rfc.get(refid.as_str()) {
            uf_union(&mut parent, a, b);
        }
        match first_citer.get(refid.as_str()) {
            Some(&first) => uf_union(&mut parent, first, a),
            None => {
                first_citer.insert(refid.as_str(), a);
            }
        }
    }

    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..msgs.len() {
        let root = uf_find(&mut parent, i);
        groups.entry(root).or_default().push(i);
    }

    groups
        .into_values()
        .map(|members| {
            // Thread root = earliest message (NULL sent_at last, then min id).
            let root = *members
                .iter()
                .min_by(|&&a, &&b| {
                    (msgs[a].sent_at.is_none(), msgs[a].sent_at, &msgs[a].id).cmp(&(
                        msgs[b].sent_at.is_none(),
                        msgs[b].sent_at,
                        &msgs[b].id,
                    ))
                })
                .expect("group is non-empty");
            let sent: Vec<i64> = members.iter().filter_map(|&m| msgs[m].sent_at).collect();
            // One conversation entry per Message-ID: the same mail synced into
            // two folders or labels (Inbox + a Gmail label, Inbox + Sent) is
            // one message to the reader. A Message-ID is unread only while
            // EVERY copy is unread -- reading either copy reads the mail, even
            // before the other copy's flag reconciles (#3458).
            let mut entries: HashMap<std::borrow::Cow<'_, str>, bool> = HashMap::new();
            for &m in &members {
                *entries.entry(msgs[m].mute_anchor()).or_insert(false) |= msgs[m].is_read;
            }
            let thread_id = format!("t:{}", msgs[root].id);
            let repoint_ids = members
                .iter()
                .filter(|&&m| msgs[m].thread_id.as_deref() != Some(thread_id.as_str()))
                .map(|&m| msgs[m].id.clone())
                .collect();
            ThreadGroupWrite {
                subject: msgs[root].subject.clone(),
                first_message_at: sent.iter().min().copied(),
                last_message_at: sent.iter().max().copied(),
                message_count: entries.len() as i64,
                unread_count: entries.values().filter(|&&read| !read).count() as i64,
                is_muted: !muted_anchors.is_empty()
                    && members
                        .iter()
                        .any(|&m| muted_anchors.contains(msgs[m].mute_anchor().as_ref())),
                member_ids: members.iter().map(|&m| msgs[m].id.clone()).collect(),
                repoint_ids,
                thread_id,
            }
        })
        .collect()
}

/// Write one [`ThreadGroupWrite`] IN PLACE: upsert its `threads` row (an
/// existing id is UPDATEd, and only when an aggregate actually changed) and
/// re-point just the members whose thread changed. Never deletes and
/// re-inserts a surviving thread row, so nothing hanging off it by foreign
/// key -- `thread_extractions` is `ON DELETE CASCADE` -- is lost to a
/// rebuild that keeps the thread (, PR #181 review). The
/// caller runs [`repoint_thread_extractions`] on whatever old thread ids its
/// members left behind, BEFORE sweeping them with
/// [`sweep_emptied_threads`] in the same transaction, so a root-changing
/// merge or split still keeps its AI extractions.
fn write_thread_group(
    tx: &rusqlite::Transaction<'_>,
    account_id: &str,
    g: &ThreadGroupWrite,
) -> rusqlite::Result<()> {
    tx.prepare_cached(
        "INSERT INTO threads
             (id, account_id, subject, first_message_at, last_message_at,
              message_count, unread_count, is_muted)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET
             subject = excluded.subject,
             first_message_at = excluded.first_message_at,
             last_message_at = excluded.last_message_at,
             message_count = excluded.message_count,
             unread_count = excluded.unread_count,
             is_muted = excluded.is_muted
         WHERE threads.subject IS NOT excluded.subject
            OR threads.first_message_at IS NOT excluded.first_message_at
            OR threads.last_message_at IS NOT excluded.last_message_at
            OR threads.message_count IS NOT excluded.message_count
            OR threads.unread_count IS NOT excluded.unread_count
            OR threads.is_muted IS NOT excluded.is_muted",
    )?
    .execute(params![
        g.thread_id,
        account_id,
        g.subject,
        g.first_message_at,
        g.last_message_at,
        g.message_count,
        g.unread_count,
        g.is_muted
    ])?;
    let mut upd_msg = tx.prepare_cached("UPDATE messages SET thread_id = ?1 WHERE id = ?2")?;
    for id in &g.repoint_ids {
        upd_msg.execute(params![g.thread_id, id])?;
    }
    Ok(())
}

/// Re-point `thread_extractions` off every id in `gone` and onto the new
/// thread that now holds its former root message -- run AFTER
/// [`write_thread_group`] has written `groups` (so `groups`' membership
/// reflects where every message landed THIS pass) but BEFORE
/// [`sweep_emptied_threads`] deletes `gone`: a merge or
/// split that changes a thread's root changes its `t:<root>` id, so the id
/// in `gone` is never reused, and `thread_extractions` being `ON DELETE
/// CASCADE` (migration 0033) would otherwise silently drop that thread's
/// AI extractions on every regroup, not just the ones that empty a thread
/// for real ( already fixed the id-unchanged case).
///
/// For each old id, prefers the group containing the LITERAL root message
/// (`old_id` is `t:<root message id>`) -- that is what "the new thread that
/// contains the old root's messages" means for a split, where the old
/// thread's other members can land in a different new group than the root
/// did. Falls back to the EARLIEST-by-`(sent_at, id)` other former member's
/// new group only when the root message itself is not present in `msgs`
/// (e.g. it departed in the same transaction) -- the same tie-break
/// `compute_thread_groups` uses to pick a root, so the choice is
/// deterministic and agrees between a full rebuild (`msgs` is the whole
/// account) and an incremental chunk (`msgs` is a `HashSet`-ordered batch),
/// never "whichever row `msgs` happened to list first" (PR #183 review).
/// Every id in `gone` came from some `msgs` row, so a fallback target
/// always exists.
/// Sort key mirroring `compute_thread_groups`' root selection (NULL
/// `sent_at` sorts last, then by id) -- `repoint_thread_extractions`'s
/// fallback picks the former thread member with the smallest key,
/// deterministically, instead of whichever `msgs` row it saw first.
type EarliestKey<'a> = (bool, Option<i64>, &'a str);

fn repoint_thread_extractions(
    tx: &rusqlite::Transaction<'_>,
    msgs: &[ThreadRow],
    groups: &[ThreadGroupWrite],
    gone: &[String],
) -> rusqlite::Result<()> {
    if gone.is_empty() {
        return Ok(());
    }
    let gone_set: HashSet<&str> = gone.iter().map(String::as_str).collect();
    let mut new_home: HashMap<&str, &str> = HashMap::new();
    for g in groups {
        for m in &g.member_ids {
            new_home.insert(m.as_str(), g.thread_id.as_str());
        }
    }
    let mut root_target: HashMap<&str, &str> = HashMap::new();
    // Keyed by old id -> (its smallest EarliestKey seen so far, its new home).
    let mut earliest: HashMap<&str, (EarliestKey<'_>, &str)> = HashMap::new();
    for m in msgs {
        let Some(old_id) = m.thread_id.as_deref() else {
            continue;
        };
        if !gone_set.contains(old_id) {
            continue;
        }
        let Some(&new_id) = new_home.get(m.id.as_str()) else {
            continue;
        };
        let key = (m.sent_at.is_none(), m.sent_at, m.id.as_str());
        earliest
            .entry(old_id)
            .and_modify(|cur| {
                if key < cur.0 {
                    *cur = (key, new_id);
                }
            })
            .or_insert((key, new_id));
        if old_id.strip_prefix("t:") == Some(m.id.as_str()) {
            root_target.insert(old_id, new_id);
        }
    }

    for old_id in gone {
        let target = root_target
            .get(old_id.as_str())
            .copied()
            .or_else(|| earliest.get(old_id.as_str()).map(|&(_, new_id)| new_id));
        let Some(new_id) = target else { continue };
        if new_id != old_id.as_str() {
            merge_thread_extractions(tx, old_id, new_id)?;
            repoint_thread_references(tx, old_id, new_id)?;
        }
    }
    Ok(())
}

/// Re-key everything else that names a conversation by its `t:<root>` id
/// from `old_id` to `new_id` (/#3458): project / work-item links
/// (either endpoint), suggestions and per-thread AI outputs. The id is derived
/// from the root message, so it moves whenever the root does (an earlier copy
/// arrives, hydration swaps INTERNALDATE for the Date header, the root is
/// archived); without this the user's links silently pointed at a dead id.
/// A row that would collide with one already on `new_id` (the UNIQUE keys of
/// `links` and `ai_outputs`) is dropped: the surviving row already says the
/// same thing.
fn repoint_thread_references(
    tx: &rusqlite::Transaction<'_>,
    old_id: &str,
    new_id: &str,
) -> rusqlite::Result<()> {
    for (type_col, id_col) in [("src_type", "src_id"), ("dst_type", "dst_id")] {
        tx.execute(
            &format!(
                "UPDATE OR IGNORE links SET {id_col} = ?1
                 WHERE {type_col} = 'thread' AND {id_col} = ?2"
            ),
            params![new_id, old_id],
        )?;
        // Whatever the UNIQUE key refused to move duplicates a link already
        // on `new_id`.
        tx.execute(
            &format!("DELETE FROM links WHERE {type_col} = 'thread' AND {id_col} = ?1"),
            params![old_id],
        )?;
    }
    tx.execute(
        "UPDATE suggestions SET thread_id = ?1 WHERE thread_id = ?2",
        params![new_id, old_id],
    )?;
    tx.execute(
        "UPDATE OR IGNORE ai_outputs SET owner_id = ?1
         WHERE scope = 'thread' AND owner_id = ?2",
        params![new_id, old_id],
    )?;
    Ok(())
}

/// Move `thread_extractions` from `old_id` (about to be swept) onto
/// `new_id` (a surviving group). There is no per-item identity across two
/// extraction runs (same reasoning as `replace_thread_extractions`), so
/// when `new_id` already carries its own job -- either it kept its own AI
/// extractions across the regroup, or an earlier `old_id` in this same pass
/// already landed one on it -- the two jobs are never interleaved: whole
/// job with the latest `created_at` wins (ties favour the id already on
/// `new_id`), the other's rows are dropped. `thread_extractions` has no
/// staleness/version column to mark instead (migration 0033); this is the
/// documented trade-off for that gap.
fn merge_thread_extractions(
    tx: &rusqlite::Transaction<'_>,
    old_id: &str,
    new_id: &str,
) -> rusqlite::Result<()> {
    let old_newest: Option<String> = tx.query_row(
        "SELECT MAX(created_at) FROM thread_extractions WHERE thread_id = ?1",
        params![old_id],
        |r| r.get(0),
    )?;
    let Some(old_newest) = old_newest else {
        return Ok(()); // old_id has no extraction job -- nothing to move.
    };
    let new_newest: Option<String> = tx.query_row(
        "SELECT MAX(created_at) FROM thread_extractions WHERE thread_id = ?1",
        params![new_id],
        |r| r.get(0),
    )?;
    match new_newest {
        Some(new_newest) if new_newest >= old_newest => {
            // new_id's own job is at least as fresh -- drop old_id's rows
            // rather than shadow it.
            tx.execute(
                "DELETE FROM thread_extractions WHERE thread_id = ?1",
                params![old_id],
            )?;
        }
        Some(_) => {
            // old_id's job is newer: it replaces whatever new_id had.
            tx.execute(
                "DELETE FROM thread_extractions WHERE thread_id = ?1",
                params![new_id],
            )?;
            tx.execute(
                "UPDATE thread_extractions SET thread_id = ?1 WHERE thread_id = ?2",
                params![new_id, old_id],
            )?;
        }
        None => {
            // new_id has nothing yet -- a plain re-point.
            tx.execute(
                "UPDATE thread_extractions SET thread_id = ?1 WHERE thread_id = ?2",
                params![new_id, old_id],
            )?;
        }
    }
    Ok(())
}

/// One incremental-rebuild chunk, inside the caller's transaction (PR #181
/// review): `ids` is a union of WHOLE connected groups (closed under
/// References, rfc822 duplicates and current thread membership -- see
/// `expand_thread_closure`), read here, in the same transaction that
/// writes them. Upserts their threads, sweeps the old thread ids that no
/// member points at any more, and clears the dirty rows of `ids` that
/// `seed_gens` (the pass's snapshot) names at an unchanged `gen` -- a row
/// written or bumped after the snapshot stays for the next pass.
fn rewrite_thread_chunk(
    tx: &rusqlite::Transaction<'_>,
    account_id: &str,
    ids: &HashSet<String>,
    seed_gens: &HashMap<&str, i64>,
) -> rusqlite::Result<()> {
    let ids: Vec<String> = ids.iter().cloned().collect();
    let msgs = load_thread_rows_for_ids(tx, account_id, &ids)?;
    let refs = load_thread_refs_for_ids(tx, &ids)?;
    let old_thread_ids: HashSet<&str> =
        msgs.iter().filter_map(|m| m.thread_id.as_deref()).collect();

    // S42: read inside this chunk's transaction, so a `mute_thread` that
    // committed before it is reflected here and one that commits after it
    // finds the thread ids this chunk wrote (both hold the connection mutex).
    let muted_anchors = load_account_muted_anchors(tx, account_id)?;
    let groups = compute_thread_groups(&msgs, &refs, &muted_anchors);
    for group in &groups {
        write_thread_group(tx, account_id, group)?;
    }
    let new_ids: HashSet<&str> = groups.iter().map(|g| g.thread_id.as_str()).collect();
    let gone: Vec<String> = old_thread_ids
        .into_iter()
        .filter(|id| !new_ids.contains(id))
        .map(str::to_string)
        .collect();
    repoint_thread_extractions(tx, &msgs, &groups, &gone)?;
    sweep_emptied_threads(tx, account_id, &gone)?;

    let mut clear = tx.prepare_cached(
        "DELETE FROM thread_dirty_messages
         WHERE account_id = ?1 AND message_id = ?2 AND gen = ?3",
    )?;
    for id in &ids {
        if let Some(gen) = seed_gens.get(id.as_str()) {
            clear.execute(params![account_id, id, gen])?;
        }
    }
    Ok(())
}

/// Every message in `account_id`, for a full [`Database::rebuild_threads`].
fn load_account_thread_rows(
    tx: &rusqlite::Transaction<'_>,
    account_id: &str,
) -> rusqlite::Result<Vec<ThreadRow>> {
    tx.prepare(
        "SELECT m.id, m.rfc822_message_id, m.sent_at, m.subject, COALESCE(f.is_read, 0),
                m.thread_id
         FROM messages m LEFT JOIN message_flags f ON f.message_id = m.id
         WHERE m.account_id = ?1",
    )?
    .query_map(params![account_id], |r| {
        Ok(ThreadRow {
            id: r.get(0)?,
            rfc: r.get(1)?,
            sent_at: r.get(2)?,
            subject: r.get(3)?,
            is_read: r.get(4)?,
            thread_id: r.get(5)?,
        })
    })?
    .collect()
}

/// Every `thread_mutes` anchor for `account_id` (S42) -- a user-sized set
/// (one row per member of each muted conversation), so both the full
/// rebuild and each incremental chunk load it whole rather than per group.
fn load_account_muted_anchors(
    conn: &rusqlite::Connection,
    account_id: &str,
) -> rusqlite::Result<HashSet<String>> {
    conn.prepare_cached("SELECT anchor FROM thread_mutes WHERE account_id = ?1")?
        .query_map(params![account_id], |r| r.get::<_, String>(0))?
        .collect()
}

/// Every reference row for a message in `account_id`, for a full
/// [`Database::rebuild_threads`].
fn load_account_thread_refs(
    tx: &rusqlite::Transaction<'_>,
    account_id: &str,
) -> rusqlite::Result<Vec<(String, String)>> {
    tx.prepare(
        "SELECT mr.message_id, mr.ref_message_id
         FROM message_references mr JOIN messages m ON m.id = mr.message_id
         WHERE m.account_id = ?1",
    )?
    .query_map(params![account_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?
    .collect()
}

/// [`load_account_thread_rows`] restricted to `ids`, chunked so no single
/// `IN (...)` query exceeds `IN_CLAUSE_CHUNK` bound parameters -- the
/// incremental engine's read of a (proven-closed) closure.
fn load_thread_rows_for_ids(
    conn: &rusqlite::Connection,
    account_id: &str,
    ids: &[String],
) -> rusqlite::Result<Vec<ThreadRow>> {
    let mut out = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(IN_CLAUSE_CHUNK) {
        let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT m.id, m.rfc822_message_id, m.sent_at, m.subject, COALESCE(f.is_read, 0),
                    m.thread_id
             FROM messages m LEFT JOIN message_flags f ON f.message_id = m.id
             WHERE m.account_id = ? AND m.id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let p =
            params_from_iter(std::iter::once(account_id).chain(chunk.iter().map(String::as_str)));
        let rows = stmt.query_map(p, |r| {
            Ok(ThreadRow {
                id: r.get(0)?,
                rfc: r.get(1)?,
                sent_at: r.get(2)?,
                subject: r.get(3)?,
                is_read: r.get(4)?,
                thread_id: r.get(5)?,
            })
        })?;
        for row in rows {
            out.push(row?);
        }
    }
    Ok(out)
}

/// [`load_account_thread_refs`] restricted to references FROM `ids` --
/// `message_references.message_id` already implies the account (every
/// message belongs to exactly one), so unlike the row loader above this
/// needs no `account_id` filter.
fn load_thread_refs_for_ids(
    conn: &rusqlite::Connection,
    ids: &[String],
) -> rusqlite::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for chunk in ids.chunks(IN_CLAUSE_CHUNK) {
        let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT message_id, ref_message_id FROM message_references WHERE message_id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let p = params_from_iter(chunk.iter().map(String::as_str));
        let rows = stmt.query_map(p, |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            out.push(row?);
        }
    }
    Ok(out)
}

/// The distinct, non-NULL `thread_id`s currently held by any message in
/// `ids`.
fn load_thread_ids_for_message_ids(
    conn: &rusqlite::Connection,
    account_id: &str,
    ids: &[String],
) -> rusqlite::Result<Vec<String>> {
    let mut out: HashSet<String> = HashSet::new();
    for chunk in ids.chunks(IN_CLAUSE_CHUNK) {
        let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT DISTINCT thread_id FROM messages
             WHERE account_id = ? AND thread_id IS NOT NULL AND id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let p =
            params_from_iter(std::iter::once(account_id).chain(chunk.iter().map(String::as_str)));
        let rows = stmt.query_map(p, |r| r.get::<_, String>(0))?;
        for row in rows {
            out.insert(row?);
        }
    }
    Ok(out.into_iter().collect())
}

/// Expand `seeds` (message ids a write just touched) to the full connected
/// component(s) they belong to under the SAME graph `compute_thread_groups`
/// unions over (rfc822 duplicates + References, both directions) UNION each
/// seed's CURRENT thread membership -- so a change that is about to
/// disconnect a message from its old thread still pulls in that thread's
/// remaining members, whose aggregates need recomputing too even though
/// they, individually, changed nothing. Iterates to a fixed point: each
/// round can grow the set via either the graph or old-thread membership,
/// and either kind of growth can in turn need the other kind next round
/// (a newly-discovered message may belong to yet another old thread; a
/// newly-absorbed old-thread sibling may reference something new).
///
/// Returns `None` if the closure exceeds `INCREMENTAL_CLOSURE_LIMIT` before
/// reaching a fixed point -- the caller falls back to a full rebuild rather
/// than either give a wrong (partial) answer or chase an unbounded
/// mailing-list thread. Bounded in practice by the ACTUAL size of the
/// threads touched, not the account: a flag flip on an ordinary reply chain
/// visits a handful of rows, never the whole mailbox.
fn expand_thread_closure(
    conn: &rusqlite::Connection,
    account_id: &str,
    seeds: &[String],
) -> rusqlite::Result<Option<HashSet<String>>> {
    let mut closure: HashSet<String> = seeds.iter().cloned().collect();
    loop {
        if closure.len() > INCREMENTAL_CLOSURE_LIMIT {
            return Ok(None);
        }
        let ids: Vec<String> = closure.iter().cloned().collect();
        let mut added = false;

        // (a) Old-thread absorption.
        let old_thread_ids = load_thread_ids_for_message_ids(conn, account_id, &ids)?;
        for chunk in old_thread_ids.chunks(IN_CLAUSE_CHUNK) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            // `INDEXED BY`: without the hint SQLite chose idx_messages_list
            // (account_id=?), an O(account size) scan, over
            // idx_messages_thread's O(matches) seek -- confirmed via EXPLAIN
            // QUERY PLAN against a seeded 100k profile (this one query was
            // most of a ~460ms "incremental" rebuild that should cost
            // single-digit ms for a 20-message closure).
            let sql = format!(
                "SELECT id FROM messages INDEXED BY idx_messages_thread
                 WHERE account_id = ? AND thread_id IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let p = params_from_iter(
                std::iter::once(account_id).chain(chunk.iter().map(String::as_str)),
            );
            let rows = stmt.query_map(p, |r| r.get::<_, String>(0))?;
            for row in rows {
                if closure.insert(row?) {
                    added = true;
                }
            }
        }

        // (b) rfc822 duplicates + References, both directions.
        let mut own_rfc: HashSet<String> = HashSet::new();
        for chunk in ids.chunks(IN_CLAUSE_CHUNK) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT DISTINCT rfc822_message_id FROM messages
                 WHERE account_id = ? AND rfc822_message_id IS NOT NULL AND id IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let p = params_from_iter(
                std::iter::once(account_id).chain(chunk.iter().map(String::as_str)),
            );
            let rows = stmt.query_map(p, |r| r.get::<_, String>(0))?;
            for row in rows {
                own_rfc.insert(row?);
            }
        }
        let mut target_rfc: HashSet<String> = own_rfc.clone();
        // Every id the closure cites, present locally or not: other messages
        // citing one of them are connected too (see `compute_thread_groups`).
        let mut cited_rfc: HashSet<String> = own_rfc.clone();
        for chunk in ids.chunks(IN_CLAUSE_CHUNK) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT DISTINCT ref_message_id FROM message_references WHERE message_id IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let p = params_from_iter(chunk.iter().map(String::as_str));
            let rows = stmt.query_map(p, |r| r.get::<_, String>(0))?;
            for row in rows {
                let r = row?;
                cited_rfc.insert(r.clone());
                target_rfc.insert(r);
            }
        }
        let target_rfc_list: Vec<String> = target_rfc.into_iter().collect();
        for chunk in target_rfc_list.chunks(IN_CLAUSE_CHUNK) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            // Same INDEXED BY reasoning as (a) above, for idx_messages_rfc822.
            let sql = format!(
                "SELECT id FROM messages INDEXED BY idx_messages_rfc822
                 WHERE account_id = ? AND rfc822_message_id IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let p = params_from_iter(
                std::iter::once(account_id).chain(chunk.iter().map(String::as_str)),
            );
            let rows = stmt.query_map(p, |r| r.get::<_, String>(0))?;
            for row in rows {
                if closure.insert(row?) {
                    added = true;
                }
            }
        }
        let own_rfc_list: Vec<String> = cited_rfc.into_iter().collect();
        for chunk in own_rfc_list.chunks(IN_CLAUSE_CHUNK) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            // Lead with the (small) IN-list side via idx_message_references_ref
            // instead of letting the planner scan every message in the
            // account first (same class of bug as (a)/(b) above).
            let sql = format!(
                "SELECT mr.message_id FROM message_references mr INDEXED BY idx_message_references_ref
                 JOIN messages m ON m.id = mr.message_id
                 WHERE m.account_id = ? AND mr.ref_message_id IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let p = params_from_iter(
                std::iter::once(account_id).chain(chunk.iter().map(String::as_str)),
            );
            let rows = stmt.query_map(p, |r| r.get::<_, String>(0))?;
            for row in rows {
                if closure.insert(row?) {
                    added = true;
                }
            }
        }

        if !added {
            return Ok(Some(closure));
        }
    }
}

/// The exact `SELECT` column list every `MessageHeader` query uses, so
/// `list_message_headers`/`list_messages_by_role` (LIMIT/OFFSET) and
/// `list_page.rs`'s keyset queries (S64) can never drift into reading
/// different columns for the same row shape. Column order must match
/// `row_to_header` positionally.
pub(crate) const HEADER_COLUMNS: &str = "m.id, m.account_id, m.mailbox_id, m.subject, \
    m.from_name, m.from_address, m.sent_at, m.snippet, m.has_attachments, \
    COALESCE(f.is_read, 0), COALESCE(t.message_count, 1), COALESCE(f.is_starred, 0), \
    b.message_id IS NOT NULL, COALESCE(t.is_muted, 0), EXISTS(SELECT 1 FROM vip_senders v WHERE v.address_key = lower(trim(m.from_address)))";

// pub(crate): reused by rules.rs (S40) so a filter-backed list and the
// existing list/search queries share one row-building path (memo §4).
///
/// Column 13 is `COALESCE(t.is_muted, 0)` (S42) — every SELECT feeding this
/// must append it right after `b.message_id IS NOT NULL` (the existing
/// `LEFT JOIN threads t` every such query already carries for
/// `thread_count`), so any query-specific trailing columns after it keep a
/// stable +1 shift rather than each picking its own index. `keywords` is
/// NOT populated here — see `attach_keywords` (no per-row join, to avoid
/// multiplying rows on a message with several keywords).
pub(crate) fn row_to_header(r: &rusqlite::Row<'_>) -> rusqlite::Result<MessageHeader> {
    Ok(MessageHeader {
        id: r.get(0)?,
        account_id: r.get(1)?,
        mailbox_id: r.get(2)?,
        subject: r.get(3)?,
        from_name: r.get(4)?,
        from_address: r.get(5)?,
        sent_at: r.get(6)?,
        snippet: r.get(7)?,
        has_attachments: r.get(8)?,
        is_read: r.get(9)?,
        thread_count: r.get(10)?,
        is_starred: r.get(11)?,
        has_body: r.get(12)?,
        keywords: Vec::new(),
        thread_muted: r.get(13)?,
        from_vip: r.get(14)?,
        to_display: None,
    })
}

/// `(account_id, mailbox path, provider_uid, mailbox_id)` of a message that
/// lives in Trash (see `Database::message_trash_state`).
type TrashedMessage = (String, String, Option<i64>, String);
/// The same row plus the mailbox role, as read from SQLite.
type TrashedMessageRow = (String, String, Option<i64>, String, Option<String>);

#[cfg(test)]
mod tests {

    /// The Swift app decides whether to show the "Enable Writes…" affordance by
    /// matching this prefix on the core's error reason (see
    /// `AccountWritePolicy.isEvaluationModeRefusal`). Renaming the message
    /// without updating the app would silently downgrade that dialog back to a
    /// raw error string, so the contract is pinned on both sides.
    #[test]
    fn evaluation_mode_message_keeps_the_prefix_the_swift_ui_matches() {
        // Kept as a literal on purpose: this is the exact string the Swift
        // side hard-codes, so the two must be compared, not derived.
        assert!(
            EVALUATION_MODE_MSG.starts_with("evaluation mode:"),
            "EVALUATION_MODE_MSG must keep the prefix the Swift UI matches: {EVALUATION_MODE_MSG}"
        );
        // Credential-free and single-line: it is shown to the user verbatim.
        assert!(!EVALUATION_MODE_MSG.contains('\n'));
    }

    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn fresh_db() -> Database {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-repo-test-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        Database::open(&path).expect("open")
    }

    fn seed_account_mailbox(db: &Database) {
        db.upsert_account(&Account {
            id: "acc1".into(),
            email: "test@local.test".into(),
            display_name: Some("Test".into()),
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
        // Mutation tests exercise remote ops, so leave evaluation mode. Tests
        // that assert the read-only DEFAULT seed their own account instead.
        db.set_account_write_enabled("acc1", true).unwrap();
    }

    fn new_test_draft() -> NewDraft {
        NewDraft {
            id: None,
            account_id: "acc1".into(),
            to: vec![DraftAddress {
                name: None,
                email: "bob@example.com".into(),
            }],
            cc: vec![],
            bcc: vec![],
            subject: "s".into(),
            body_text: "b".into(),
            // Markdown off (S46) and no identity override (S47) — this helper
            // predates both, and its callers assert the plain-draft path.
            body_html: None,
            from_email: None,
            from_name: None,
            in_reply_to: None,
            references_header: None,
        }
    }

    #[test]
    fn draft_attachment_round_trips_metadata_and_bytes() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let draft_id = db.save_draft(&new_test_draft()).unwrap();
        assert!(db.list_draft_attachments(draft_id).unwrap().is_empty());
        let aid = db
            .add_draft_attachment(
                draft_id,
                "report.pdf",
                "application/pdf",
                false,
                None,
                b"hello",
            )
            .unwrap();
        let listed = db.list_draft_attachments(draft_id).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, aid);
        assert_eq!(listed[0].filename, "report.pdf");
        assert_eq!(listed[0].size_bytes, 5);
        assert!(!listed[0].is_inline);
        assert_eq!(
            db.draft_attachment_bytes(aid).unwrap().as_deref(),
            Some(&b"hello"[..])
        );
        db.remove_draft_attachment(draft_id, aid).unwrap();
        assert!(db.list_draft_attachments(draft_id).unwrap().is_empty());
        assert_eq!(db.draft_attachment_bytes(aid).unwrap(), None);
    }

    #[test]
    fn draft_attachments_cascade_with_their_draft() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let draft_id = db.save_draft(&new_test_draft()).unwrap();
        let aid = db
            .add_draft_attachment(draft_id, "a.png", "image/png", true, Some("cid1"), b"data")
            .unwrap();
        db.delete_draft(draft_id).unwrap();
        assert_eq!(db.draft_attachment_bytes(aid).unwrap(), None);
        assert!(db.list_draft_attachments(draft_id).unwrap().is_empty());
    }

    #[test]
    fn add_draft_attachment_refuses_once_the_draft_is_no_longer_editable() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let draft_id = db.save_draft(&new_test_draft()).unwrap();
        db.send_draft(draft_id).unwrap(); // status -> outbox
        let err = db
            .add_draft_attachment(draft_id, "late.pdf", "application/pdf", false, None, b"x")
            .unwrap_err();
        assert!(err.to_string().contains("draft"), "{err}");
        let err = db.remove_draft_attachment(draft_id, 1).unwrap_err();
        assert!(err.to_string().contains("draft"), "{err}");
    }

    #[test]
    fn set_draft_attachment_inline_flips_and_refuses_non_editable_drafts() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let draft_id = db.save_draft(&new_test_draft()).unwrap();
        let att = db
            .add_draft_attachment(
                draft_id,
                "p.png",
                "application/octet-stream",
                false,
                None,
                b"x",
            )
            .unwrap();
        db.set_draft_attachment_inline(draft_id, att, Some(("img-aa@castellum", "image/png")))
            .unwrap();
        let rows = db.list_draft_attachments(draft_id).unwrap();
        assert!(rows[0].is_inline);
        assert_eq!(rows[0].content_id.as_deref(), Some("img-aa@castellum"));
        assert_eq!(rows[0].content_type, "image/png");
        db.set_draft_attachment_inline(draft_id, att, None).unwrap();
        let rows = db.list_draft_attachments(draft_id).unwrap();
        assert!(!rows[0].is_inline && rows[0].content_id.is_none());
        // Not this draft's attachment: an error, nothing changes.
        let err = db
            .set_draft_attachment_inline(draft_id, att + 100, None)
            .unwrap_err();
        assert!(err.to_string().contains("unknown attachment"), "{err}");
        // Once queued for sending the draft is read-only.
        db.send_draft(draft_id).unwrap();
        let err = db
            .set_draft_attachment_inline(draft_id, att, None)
            .unwrap_err();
        assert!(err.to_string().contains("draft"), "{err}");
    }

    #[test]
    fn enqueue_eml_import_queues_an_operation_and_blob() {
        let db = fresh_db();
        seed_account_mailbox(&db); // acc1 + acc1:INBOX (path "INBOX")

        let op_id = db
            .enqueue_eml_import("acc1", "acc1:INBOX", "<synth@x>", b"From: a@x\r\n\r\nbody")
            .unwrap();

        let ops = db.pending_operations("acc1").unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].id, op_id);
        assert_eq!(ops[0].kind, "import_eml");
        let payload: serde_json::Value = serde_json::from_str(&ops[0].payload_json).unwrap();
        assert_eq!(payload["dest_path"], "INBOX");
        assert_eq!(payload["message_id"], "<synth@x>");

        let blob = db.operation_blob(op_id).unwrap().unwrap();
        assert_eq!(blob, b"From: a@x\r\n\r\nbody");
    }

    #[test]
    fn enqueue_eml_import_refuses_a_mailbox_from_another_account() {
        let db = fresh_db();
        seed_account_mailbox(&db); // acc1:INBOX
        db.upsert_account(&Account {
            id: "acc2".into(),
            email: "other@local.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();

        let err = db
            .enqueue_eml_import("acc2", "acc1:INBOX", "<x@x>", b"raw")
            .unwrap_err();
        assert!(err.0.contains("does not belong"), "got: {}", err.0);
        assert!(db.pending_operations("acc2").unwrap().is_empty());
    }

    #[test]
    fn account_write_policy_is_fail_closed_and_toggles() {
        let db = fresh_db();
        // Unknown account: fail-closed (read-only).
        assert!(!db.account_write_enabled("ghost").unwrap());
        db.upsert_account(&Account {
            id: "acc1".into(),
            email: "test@local.test".into(),
            display_name: Some("Test".into()),
            provider: "imap".into(),
        })
        .unwrap();
        // Newly-added account defaults to read-only (evaluation mode).
        assert!(
            !db.account_write_enabled("acc1").unwrap(),
            "new accounts default to evaluation mode"
        );
        // Re-upserting the account (e.g. on sync) must NOT reset the policy.
        db.set_account_write_enabled("acc1", true).unwrap();
        db.upsert_account(&Account {
            id: "acc1".into(),
            email: "test@local.test".into(),
            display_name: Some("Changed".into()),
            provider: "imap".into(),
        })
        .unwrap();
        assert!(
            db.account_write_enabled("acc1").unwrap(),
            "upsert_account preserves the write policy"
        );
        // Toggle back off.
        db.set_account_write_enabled("acc1", false).unwrap();
        assert!(!db.account_write_enabled("acc1").unwrap());
        // Toggling an unknown account errors (never a silent no-op).
        assert!(db.set_account_write_enabled("ghost", true).is_err());
    }

    #[test]
    fn delete_account_cascades_mailboxes_and_drafts_and_errors_on_unknown() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.save_draft(&draft_input("Bye for now")).unwrap();
        assert_eq!(db.list_mailboxes("acc1").unwrap().len(), 1);
        assert_eq!(db.list_drafts("acc1").unwrap().len(), 1);

        db.delete_account("acc1").unwrap();

        assert!(db.list_accounts().unwrap().is_empty());
        // FK ON DELETE CASCADE removes mailboxes with the account row.
        assert!(db.list_mailboxes("acc1").unwrap().is_empty());
        // drafts.account_id carries no FK — deleted explicitly, not orphaned.
        assert!(db.list_drafts("acc1").unwrap().is_empty());

        // Deleting again (now unknown) errors rather than silently no-op-ing.
        assert!(db.delete_account("acc1").is_err());
    }

    #[test]
    fn delete_account_cascades_message_decode_skips() {
        // Opus review of PR #180, point 4: a retry-set row for a deleted
        // account/mailbox must not linger and retry forever for nothing.
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.record_message_decode_skip(
            "acc1",
            "acc1:INBOX",
            42,
            1,
            "message could not be decoded and was skipped",
            1_000_000,
        )
        .unwrap();
        assert_eq!(
            db.count_message_decode_skips("acc1", "acc1:INBOX").unwrap(),
            1
        );

        db.delete_account("acc1").unwrap();

        assert_eq!(
            db.count_message_decode_skips("acc1", "acc1:INBOX").unwrap(),
            0,
            "FK ON DELETE CASCADE (accounts -> mailboxes -> message_decode_skips) \
             removes the row with the account"
        );
    }

    #[test]
    fn reset_account_cascades_message_decode_skips() {
        // `reset_account` deletes `mailboxes` rows directly (unlike
        // `delete_account`, which relies on the accounts FK); the SAME
        // mailboxes(id) ON DELETE CASCADE on message_decode_skips must
        // still clear the retry set.
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.record_message_decode_skip(
            "acc1",
            "acc1:INBOX",
            42,
            1,
            "message could not be decoded and was skipped",
            1_000_000,
        )
        .unwrap();

        db.reset_account("acc1", false).unwrap();

        assert_eq!(
            db.count_message_decode_skips("acc1", "acc1:INBOX").unwrap(),
            0
        );
        // The account row itself survives a reset (unlike delete_account).
        assert_eq!(db.list_accounts().unwrap().len(), 1);
    }

    const SKIP_REASON: &str = "message could not be decoded and was skipped";

    #[test]
    fn a_new_decode_skip_is_first_due_an_hour_later() {
        // Review of PR #180, finding 3: a skip used to be retried on EVERY
        // pass, costing reconnects forever; now it waits for its backoff.
        let db = fresh_db();
        seed_account_mailbox(&db);
        let t0 = 1_000_000;
        db.record_message_decode_skip("acc1", "acc1:INBOX", 42, 1, SKIP_REASON, t0)
            .unwrap();
        assert_eq!(
            db.message_decode_skip_backoff("acc1", "acc1:INBOX", 1, 42)
                .unwrap(),
            Some((0, t0 + 3600))
        );
        let due = |now| {
            db.list_due_message_decode_skips("acc1", "acc1:INBOX", 1, now, 20)
                .unwrap()
        };
        assert!(due(t0).is_empty(), "not due right after the skip");
        assert!(due(t0 + 3599).is_empty());
        assert_eq!(due(t0 + 3600), vec![42]);
        assert!(
            db.list_due_message_decode_skips("acc1", "acc1:INBOX", 2, t0 + 3600, 20)
                .unwrap()
                .is_empty(),
            "another UID space's rows are never due"
        );
    }

    #[test]
    fn a_failed_retry_backs_off_4h_then_24h_then_caps_at_7d() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.record_message_decode_skip("acc1", "acc1:INBOX", 42, 1, SKIP_REASON, 0)
            .unwrap();
        let mut now = 3600;
        let mut delays = Vec::new();
        for _ in 0..5 {
            db.record_message_decode_retry_failure("acc1", "acc1:INBOX", 1, 42, now)
                .unwrap();
            let (_, next) = db
                .message_decode_skip_backoff("acc1", "acc1:INBOX", 1, 42)
                .unwrap()
                .unwrap();
            delays.push(next - now);
            now = next;
        }
        assert_eq!(
            delays,
            vec![4 * 3600, 24 * 3600, 7 * 86400, 7 * 86400, 7 * 86400]
        );
        assert_eq!(
            db.message_decode_skip_backoff("acc1", "acc1:INBOX", 1, 42)
                .unwrap()
                .map(|(attempts, _)| attempts),
            Some(5)
        );
    }

    #[test]
    fn a_repeat_skip_during_import_keeps_the_existing_backoff() {
        // An import that re-skips a UID already in the set (e.g. after its
        // checkpoint was cleared) must not reset it to "due in an hour".
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.record_message_decode_skip("acc1", "acc1:INBOX", 42, 1, SKIP_REASON, 0)
            .unwrap();
        db.record_message_decode_retry_failure("acc1", "acc1:INBOX", 1, 42, 3600)
            .unwrap();
        db.record_message_decode_skip("acc1", "acc1:INBOX", 42, 1, SKIP_REASON, 5000)
            .unwrap();
        assert_eq!(
            db.message_decode_skip_backoff("acc1", "acc1:INBOX", 1, 42)
                .unwrap(),
            Some((1, 3600 + 4 * 3600))
        );
        assert_eq!(
            db.count_message_decode_skips("acc1", "acc1:INBOX").unwrap(),
            1
        );
    }

    #[test]
    fn due_skips_are_listed_most_overdue_first_up_to_the_limit() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        for (uid, recorded_at) in [(5, 300), (9, 100), (7, 200), (8, 900_000)] {
            db.record_message_decode_skip("acc1", "acc1:INBOX", uid, 1, SKIP_REASON, recorded_at)
                .unwrap();
        }
        assert_eq!(
            db.list_due_message_decode_skips("acc1", "acc1:INBOX", 1, 10_000, 20)
                .unwrap(),
            vec![9, 7, 5],
            "oldest due first (not lowest UID first), and 8 is not due yet"
        );
        assert_eq!(
            db.list_due_message_decode_skips("acc1", "acc1:INBOX", 1, 10_000, 2)
                .unwrap(),
            vec![9, 7]
        );
    }

    #[test]
    fn reset_blocked_message_keeps_the_prefix_the_swift_ui_matches() {
        assert!(RESET_BLOCKED_MSG_PREFIX.starts_with("reset blocked:"));
        assert!(!RESET_BLOCKED_MSG_PREFIX.contains('\n'));
    }

    #[test]
    fn mailbox_removal_blocked_message_keeps_the_prefix_the_swift_ui_matches() {
        assert!(MAILBOX_REMOVAL_BLOCKED_MSG_PREFIX.starts_with("mailbox removal blocked:"));
        assert!(!MAILBOX_REMOVAL_BLOCKED_MSG_PREFIX.contains('\n'));
    }

    #[test]
    fn reset_account_returns_only_cache_hashes_no_other_account_references() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_account(&Account {
            id: "acc2".into(),
            email: "two@local.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.upsert_mailbox(&Mailbox {
            id: "acc2:INBOX".into(),
            account_id: "acc2".into(),
            path: "INBOX".into(),
            name: "INBOX".into(),
            role: Some("inbox".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.upsert_message(&msg(1, "a1", 1000)).unwrap();
        db.upsert_message(&msg(2, "a1b", 1001)).unwrap();
        let mut other = msg(1, "b1", 1002);
        other.id = "acc2:INBOX:1".into();
        other.account_id = "acc2".into();
        other.mailbox_id = "acc2:INBOX".into();
        db.upsert_message(&other).unwrap();
        db.upsert_attachment_cache_entry("acc1:INBOX:1", 0, "only-acc1", 10, 1)
            .unwrap();
        db.upsert_attachment_cache_entry("acc1:INBOX:2", 0, "shared", 20, 2)
            .unwrap();
        db.upsert_attachment_cache_entry("acc2:INBOX:1", 0, "shared", 20, 3)
            .unwrap();

        let orphaned = db.reset_account("acc1", false).unwrap();

        assert_eq!(orphaned, vec!["only-acc1".to_string()]);
        // The other account's row (and so its file) is untouched.
        assert!(db
            .attachment_cache_entry("acc2:INBOX:1", 0)
            .unwrap()
            .is_some());
        assert!(db
            .attachment_cache_entry("acc1:INBOX:1", 0)
            .unwrap()
            .is_none());
    }

    #[test]
    fn reset_account_errors_on_an_unknown_account() {
        let db = fresh_db();
        assert!(db.reset_account("ghost", false).is_err());
        assert!(
            db.reset_account_blockers("ghost").is_ok(),
            "blockers is a benign read: 0/0"
        );
        assert_eq!(
            db.reset_account_blockers("ghost").unwrap(),
            ResetAccountBlockers {
                unsent_outbox_drafts: 0,
                pending_operations: 0
            }
        );
    }

    #[test]
    fn reset_account_refuses_with_an_unsent_outbox_draft_unless_forced() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "keep me gone", 1000)).unwrap();
        let draft_id = db.save_draft(&draft_input("still sending")).unwrap();
        db.send_draft(draft_id).unwrap(); // status -> outbox, queues a 'send' op

        let blockers = db.reset_account_blockers("acc1").unwrap();
        assert_eq!(blockers.unsent_outbox_drafts, 1);
        assert_eq!(blockers.pending_operations, 1);
        assert!(!blockers.is_empty());

        let err = db.reset_account("acc1", false).unwrap_err();
        assert!(err.0.starts_with(RESET_BLOCKED_MSG_PREFIX), "{}", err.0);
        // Nothing touched yet: the mailbox and draft both survive the refusal.
        assert_eq!(db.list_mailboxes("acc1").unwrap().len(), 1);
        assert_eq!(db.list_drafts("acc1").unwrap().len(), 1);

        db.reset_account("acc1", true).unwrap();
        assert!(db.list_mailboxes("acc1").unwrap().is_empty());
        assert!(db.list_drafts("acc1").unwrap().is_empty());
        // The account row itself survives a reset (unlike delete_account).
        assert_eq!(db.list_accounts().unwrap().len(), 1);
    }

    #[test]
    fn reset_account_wipes_local_mail_but_keeps_the_account_row_and_cursor_is_gone() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "gone", 1000)).unwrap();
        db.set_sync_cursor(
            "acc1",
            "acc1:INBOX",
            &SyncCursor {
                uid_validity: Some(1),
                uid_next: Some(2),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
        db.rebuild_threads("acc1").unwrap();
        assert!(
            !db.list_thread_summaries("acc1", 50, 0).unwrap().is_empty(),
            "sanity: a thread exists to be wiped"
        );

        db.reset_account("acc1", false).unwrap();

        assert_eq!(
            db.list_accounts().unwrap().len(),
            1,
            "the account row itself survives a reset"
        );
        assert!(db.list_mailboxes("acc1").unwrap().is_empty());
        assert!(db.get_sync_cursor("acc1", "acc1:INBOX").unwrap().is_none());
        assert!(db.list_thread_summaries("acc1", 50, 0).unwrap().is_empty());
    }

    #[test]
    fn reset_account_force_leaves_an_in_flight_operation_update_a_clean_error() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "in flight", 1000)).unwrap();
        let draft_id = db.save_draft(&draft_input("sending")).unwrap();
        db.send_draft(draft_id).unwrap();
        let op_id = db.pending_operations("acc1").unwrap()[0].id;
        db.begin_operation_attempt(op_id).unwrap(); // executor picked it up

        db.reset_account("acc1", true).unwrap();

        // The executor finishing after the reset gets an error, not a panic,
        // and the connection lock is not poisoned afterwards.
        assert!(db.set_operation_status(op_id, "succeeded", None).is_err());
        assert_eq!(db.list_accounts().unwrap().len(), 1);
        assert!(db.pending_operations("acc1").unwrap().is_empty());
        // And a fresh sync-style write works again.
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "after", 2000)).unwrap();
        assert_eq!(
            db.reset_account_blockers("acc1")
                .unwrap()
                .pending_operations,
            0
        );
    }

    /// A UIDVALIDITY reset / expunge purges messages but used to leave the
    /// cached summary and queued job keyed by the reusable `<mailbox>:<uid>` id.
    #[test]
    fn purging_messages_drops_their_ai_outputs_and_queued_jobs() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        for uid in 1..=2 {
            db.upsert_message(&msg(uid, "old", 1000)).unwrap();
        }
        db.rebuild_threads("acc1").unwrap();
        let mut job_ids = vec![];
        for uid in 1..=2 {
            let mid = format!("acc1:INBOX:{uid}");
            let job = db
                .enqueue_ai_job(
                    Some("acc1"),
                    "summarize_message",
                    &format!(r#"{{"message_id":"{mid}"}}"#),
                    0,
                )
                .unwrap();
            db.upsert_ai_output(
                job,
                "message",
                &mid,
                "summarize_message",
                "old summary",
                true,
                None,
                None,
                None,
            )
            .unwrap();
            job_ids.push(job);
        }
        // A third, queued job for a message about to vanish.
        let queued = db
            .enqueue_ai_job(
                Some("acc1"),
                "embed_message",
                r#"{"message_id":"acc1:INBOX:1"}"#,
                0,
            )
            .unwrap();

        // Single-message purge (OpState::vanish path).
        db.purge_message("acc1", "acc1:INBOX:1").unwrap();
        assert!(db
            .get_ai_output("message", "acc1:INBOX:1", "summarize_message")
            .unwrap()
            .is_none());
        assert!(db
            .get_ai_output("message", "acc1:INBOX:2", "summarize_message")
            .unwrap()
            .is_some());
        assert!(
            db.poll_ai_job(queued, 0).unwrap().is_none(),
            "queued job for the purged id is gone"
        );

        // Mailbox purge (UIDVALIDITY reset path).
        db.delete_messages_for_mailbox("acc1", "acc1:INBOX")
            .unwrap();
        assert!(db
            .get_ai_output("message", "acc1:INBOX:2", "summarize_message")
            .unwrap()
            .is_none());
    }

    #[test]
    fn reset_account_purges_ai_outputs_and_thread_links_but_keeps_the_write_policy() {
        use crate::{EntityKind, LinkEnd, LinkUpsert};

        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "ai'd", 1000)).unwrap();
        db.rebuild_threads("acc1").unwrap();
        let message_id = "acc1:INBOX:1".to_string();
        let thread_id = format!("t:{message_id}");

        let job_id = db
            .enqueue_ai_job(
                Some("acc1"),
                "summarize_message",
                &format!(r#"{{"message_id":"{message_id}"}}"#),
                0,
            )
            .unwrap();
        db.upsert_ai_output(
            job_id,
            "message",
            &message_id,
            "summarize_message",
            "a summary",
            true,
            None,
            None,
            None,
        )
        .unwrap();
        let thread_job_id = db
            .enqueue_ai_job(
                Some("acc1"),
                "summarize_thread",
                &format!(r#"{{"thread_id":"{thread_id}"}}"#),
                0,
            )
            .unwrap();
        db.upsert_ai_output(
            thread_job_id,
            "thread",
            &thread_id,
            "summarize_thread",
            "a thread summary",
            true,
            None,
            None,
            None,
        )
        .unwrap();

        // A link touching the thread (threads carry no delete trigger — 0017).
        db.upsert_link(LinkUpsert {
            src: LinkEnd::new(EntityKind::Message, &message_id),
            dst: LinkEnd::new(EntityKind::Thread, &thread_id),
            relation: "mentions",
            origin: "manual",
            provenance: None,
            confidence: None,
        })
        .unwrap();
        assert!(!db
            .links_for(LinkEnd::new(EntityKind::Thread, &thread_id))
            .unwrap()
            .is_empty());

        assert!(
            db.account_write_enabled("acc1").unwrap(),
            "sanity: seed enabled writes"
        );

        db.reset_account("acc1", false).unwrap();

        assert!(db
            .get_ai_output("message", &message_id, "summarize_message")
            .unwrap()
            .is_none());
        assert!(db
            .get_ai_output("thread", &thread_id, "summarize_thread")
            .unwrap()
            .is_none());
        assert!(
            db.links_for(LinkEnd::new(EntityKind::Thread, &thread_id)).unwrap().is_empty(),
            "a reset must not leave a dangling thread link behind (threads have no delete trigger, 0017)"
        );
        // "Settings" (the write-enabled policy) survive a reset.
        assert!(
            db.account_write_enabled("acc1").unwrap(),
            "write policy must survive a reset"
        );
    }

    fn seed_second_account_mailbox(db: &Database, account_id: &str, role: &str) {
        db.upsert_account(&Account {
            id: account_id.into(),
            email: format!("{account_id}@local.test"),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.upsert_mailbox(&Mailbox {
            id: format!("{account_id}:BOX"),
            account_id: account_id.into(),
            path: "BOX".into(),
            name: "BOX".into(),
            role: Some(role.into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
    }

    fn msg_for(
        account_id: &str,
        mailbox_id: &str,
        uid: i64,
        subject: &str,
        sent_at: i64,
    ) -> NewMessage {
        NewMessage {
            id: format!("{mailbox_id}:{uid}"),
            account_id: account_id.into(),
            mailbox_id: mailbox_id.into(),
            provider_uid: Some(uid),
            rfc822_message_id: Some(format!("<{mailbox_id}-{uid}@local.test>")),
            subject: Some(subject.into()),
            from_name: Some("Alice".into()),
            from_address: Some("alice@local.test".into()),
            sent_at: Some(sent_at),
            snippet: Some("hello".into()),
            size_bytes: Some(1234),
            has_attachments: false,
            attachments: None,
            is_read: false,
            is_starred: false,
            addresses: vec![Address {
                kind: "from".into(),
                name: Some("Alice".into()),
                address: "alice@local.test".into(),
            }],
            text_body: Some("hi".into()),
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: vec![],
        }
    }

    #[test]
    fn list_messages_by_role_spans_accounts_and_excludes_other_roles() {
        let db = fresh_db();
        seed_account_mailbox(&db); // acc1 + acc1:INBOX (role inbox)
        seed_second_account_mailbox(&db, "acc2", "inbox");
        seed_second_account_mailbox(&db, "acc3", "trash"); // different role — must be excluded

        db.upsert_message(&msg(1, "from acc1", 3000)).unwrap(); // acc1:INBOX:1
        db.upsert_message(&msg_for("acc2", "acc2:BOX", 1, "from acc2", 5000))
            .unwrap();
        db.upsert_message(&msg_for("acc3", "acc3:BOX", 1, "trash mail", 9000))
            .unwrap();

        let rows = db.list_messages_by_role("inbox", 50, 0).unwrap();
        let ids: Vec<&str> = rows.iter().map(|h| h.account_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["acc2", "acc1"],
            "newest first, across accounts, role-filtered"
        );
    }

    #[test]
    fn snoozing_a_message_hides_it_from_list_message_headers_and_list_messages_by_role() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "hide me", 1000)).unwrap();
        db.upsert_message(&msg(2, "stay visible", 2000)).unwrap();

        db.snooze_message("acc1:INBOX:1", 9_999_999_999).unwrap();

        let per_mailbox = db
            .list_message_headers("acc1", "acc1:INBOX", 50, 0)
            .unwrap();
        assert_eq!(
            per_mailbox
                .iter()
                .map(|h| h.id.as_str())
                .collect::<Vec<_>>(),
            vec!["acc1:INBOX:2"]
        );

        let unified = db.list_messages_by_role("inbox", 50, 0).unwrap();
        assert_eq!(
            unified.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
            vec!["acc1:INBOX:2"]
        );
    }

    #[test]
    fn snooze_message_errors_on_unknown_message_id() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let err = db
            .snooze_message("acc1:INBOX:missing", 9_999_999_999)
            .unwrap_err();
        assert!(err.to_string().contains("unknown message id"), "{err}");
    }

    #[test]
    fn re_snoozing_a_message_updates_its_due_time_not_a_duplicate_row() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "s", 1000)).unwrap();

        db.snooze_message("acc1:INBOX:1", 100).unwrap();
        db.snooze_message("acc1:INBOX:1", 200).unwrap();

        let rows = db.list_snoozed_messages(50, 0).unwrap();
        assert_eq!(rows.len(), 1, "re-snoozing must update, not duplicate");
        assert_eq!(rows[0].snoozed_until_epoch, 200);
    }

    #[test]
    fn unsnooze_message_restores_visibility_and_reports_whether_it_was_snoozed() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "s", 1000)).unwrap();
        db.snooze_message("acc1:INBOX:1", 9_999_999_999).unwrap();

        assert!(db.unsnooze_message("acc1:INBOX:1").unwrap());
        assert!(
            !db.unsnooze_message("acc1:INBOX:1").unwrap(),
            "second call: nothing left to unsnooze"
        );

        let rows = db
            .list_message_headers("acc1", "acc1:INBOX", 50, 0)
            .unwrap();
        assert_eq!(rows.len(), 1, "unsnoozing restores it to the normal list");
    }

    #[test]
    fn list_snoozed_messages_orders_soonest_due_first_across_accounts() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_second_account_mailbox(&db, "acc2", "inbox");
        db.upsert_message(&msg(1, "soon", 1000)).unwrap();
        db.upsert_message(&msg_for("acc2", "acc2:BOX", 1, "later", 2000))
            .unwrap();

        db.snooze_message("acc2:BOX:1", 200).unwrap();
        db.snooze_message("acc1:INBOX:1", 100).unwrap();

        let rows = db.list_snoozed_messages(50, 0).unwrap();
        assert_eq!(
            rows.iter()
                .map(|r| r.header.id.as_str())
                .collect::<Vec<_>>(),
            vec!["acc1:INBOX:1", "acc2:BOX:1"]
        );
    }

    #[test]
    fn unsnooze_due_removes_and_returns_only_rows_at_or_before_now() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "due", 1000)).unwrap();
        db.upsert_message(&msg(2, "not due", 2000)).unwrap();
        db.snooze_message("acc1:INBOX:1", 100).unwrap();
        db.snooze_message("acc1:INBOX:2", 999_999).unwrap();

        let due = db.unsnooze_due(500).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].message_id, "acc1:INBOX:1");
        assert_eq!(due[0].account_id, "acc1");

        let remaining = db.list_snoozed_messages(50, 0).unwrap();
        assert_eq!(
            remaining.len(),
            1,
            "the due row is removed, the future one stays"
        );
        assert_eq!(remaining[0].header.id, "acc1:INBOX:2");
    }

    #[test]
    fn unsnooze_due_is_idempotent_past_the_same_due_time() {
        // The sweep runs on a timer and at launch, so it WILL be called again
        // past the same due time. The second call must return nothing and must
        // not resurrect or re-report the row.
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "due", 1000)).unwrap();
        db.snooze_message("acc1:INBOX:1", 100).unwrap();

        let first = db.unsnooze_due(500).unwrap();
        assert_eq!(first.len(), 1);
        let second = db.unsnooze_due(500).unwrap();
        assert!(second.is_empty(), "a second sweep must be a no-op");
        let third = db.unsnooze_due(1_000_000).unwrap();
        assert!(third.is_empty(), "and stay a no-op at a later now()");
        assert!(db.list_snoozed_messages(50, 0).unwrap().is_empty());
    }

    #[test]
    fn a_snooze_survives_closing_and_reopening_the_profile() {
        // The stage's headline guarantee: the state lives in SQLite, not in
        // memory, so quitting before the due time must not lose the snooze.
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-snooze-restart-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);

        {
            let db = Database::open(&path).expect("open");
            seed_account_mailbox(&db);
            db.upsert_message(&msg(1, "later", 1000)).unwrap();
            db.snooze_message("acc1:INBOX:1", 5_000).unwrap();
            assert_eq!(db.list_snoozed_messages(50, 0).unwrap().len(), 1);
        } // the "app quits" here

        let db = Database::open(&path).expect("reopen");
        let still = db.list_snoozed_messages(50, 0).unwrap();
        assert_eq!(still.len(), 1, "the snooze must survive a restart");
        assert_eq!(still[0].header.id, "acc1:INBOX:1");
        assert!(
            db.unsnooze_due(4_999).unwrap().is_empty(),
            "still not due after the restart"
        );
        let due = db.unsnooze_due(5_000).unwrap();
        assert_eq!(due.len(), 1, "and it returns to the inbox when due");
        assert_eq!(due[0].message_id, "acc1:INBOX:1");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn list_messages_by_role_paginates_with_a_stable_tiebreak() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_second_account_mailbox(&db, "acc2", "inbox");
        // Two messages sharing the same sent_at, in different accounts — the
        // tiebreak must be deterministic (id, matching idx_messages_list's
        // per-mailbox convention) so pagination never skips/repeats a row.
        db.upsert_message(&msg(1, "tie a", 1000)).unwrap(); // id "acc1:INBOX:1"
        db.upsert_message(&msg_for("acc2", "acc2:BOX", 1, "tie b", 1000))
            .unwrap(); // id "acc2:BOX:1"

        let page1 = db.list_messages_by_role("inbox", 1, 0).unwrap();
        let page2 = db.list_messages_by_role("inbox", 1, 1).unwrap();
        assert_eq!(page1.len(), 1);
        assert_eq!(page2.len(), 1);
        assert_ne!(
            page1[0].id, page2[0].id,
            "must not repeat a row across pages"
        );
    }

    #[test]
    fn list_thread_summaries_across_accounts_spans_every_account() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_second_account_mailbox(&db, "acc2", "inbox");
        db.upsert_message(&msg(1, "t1", 1000)).unwrap();
        db.upsert_message(&msg_for("acc2", "acc2:BOX", 1, "t2", 2000))
            .unwrap();
        db.rebuild_threads("acc1").unwrap();
        db.rebuild_threads("acc2").unwrap();

        let rows = db.list_thread_summaries_across_accounts(50, 0).unwrap();
        let accounts: Vec<&str> = rows.iter().map(|t| t.account_id.as_str()).collect();
        assert_eq!(
            accounts,
            vec!["acc2", "acc1"],
            "newest activity first, across accounts"
        );
    }

    #[test]
    fn search_messages_across_accounts_finds_matches_in_every_account() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_second_account_mailbox(&db, "acc2", "inbox");
        db.upsert_message(&msg(1, "budget review", 1000)).unwrap();
        db.upsert_message(&msg_for("acc2", "acc2:BOX", 1, "budget forecast", 2000))
            .unwrap();
        db.upsert_message(&msg_for("acc2", "acc2:BOX", 2, "unrelated", 3000))
            .unwrap();

        let rows = db.search_messages_across_accounts("budget", 50).unwrap();
        let accounts: std::collections::HashSet<&str> =
            rows.iter().map(|h| h.account_id.as_str()).collect();
        assert_eq!(rows.len(), 2);
        assert!(accounts.contains("acc1") && accounts.contains("acc2"));
    }

    // -- hybrid search (S53) -------------------------------------------------

    fn embed_le(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|f| f.to_le_bytes()).collect()
    }

    #[test]
    fn hybrid_search_without_a_query_embedding_matches_keyword_only() {
        // The hard degrade requirement: an unreachable/unconfigured embedding
        // backend (modelled here as the caller simply having no vector to
        // pass) must return EXACTLY today's keyword results, byte-for-byte.
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "quarterly report", 3000))
            .unwrap();
        db.upsert_message(&msg(2, "quarterly summary", 2000))
            .unwrap();

        let keyword_only = db.search_messages("acc1", "quarterly", 50).unwrap();
        let hybrid_none = db
            .search_messages_hybrid(Some("acc1"), "quarterly", 50, None)
            .unwrap();
        assert_eq!(
            hybrid_none, keyword_only,
            "no query embedding => identical to search_messages"
        );
    }

    #[test]
    fn hybrid_search_across_accounts_without_a_query_embedding_matches_keyword_only() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_second_account_mailbox(&db, "acc2", "inbox");
        db.upsert_message(&msg(1, "budget review", 1000)).unwrap();
        db.upsert_message(&msg_for("acc2", "acc2:BOX", 1, "budget forecast", 2000))
            .unwrap();

        let keyword_only = db.search_messages_across_accounts("budget", 50).unwrap();
        let hybrid_none = db.search_messages_hybrid(None, "budget", 50, None).unwrap();
        assert_eq!(hybrid_none, keyword_only);
    }

    #[test]
    fn hybrid_search_with_no_keyword_matches_returns_empty_even_with_an_embedding() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "unrelated", 1000)).unwrap();

        let query_vector = vec![1.0f32, 0.0, 0.0];
        let hybrid = db
            .search_messages_hybrid(Some("acc1"), "nonexistentterm", 50, Some(&query_vector))
            .unwrap();
        assert!(hybrid.is_empty());
    }

    #[test]
    fn hybrid_search_degrades_smoothly_with_partial_embedding_coverage() {
        // A partially-embedded mailbox: keyword results stay complete, and
        // nothing errors just because most of them have no vector yet.
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "forecast alpha", 3000)).unwrap();
        db.upsert_message(&msg(2, "forecast beta", 2000)).unwrap();
        db.upsert_message(&msg(3, "forecast gamma", 1000)).unwrap();
        // Only one of the three has ever been embedded.
        db.upsert_message_embedding(
            "acc1:INBOX:2",
            "nomic-embed-text",
            3,
            &embed_le(&[0.1, 0.2, 0.3]),
        )
        .unwrap();

        let query_vector = vec![0.1f32, 0.2, 0.3];
        let hybrid = db
            .search_messages_hybrid(Some("acc1"), "forecast", 50, Some(&query_vector))
            .unwrap();
        assert_eq!(
            hybrid.len(),
            3,
            "every keyword match stays present regardless of embedding coverage"
        );
        let ids: std::collections::HashSet<&str> = hybrid.iter().map(|h| h.id.as_str()).collect();
        assert!(ids.contains("acc1:INBOX:1"));
        assert!(ids.contains("acc1:INBOX:2"));
        assert!(ids.contains("acc1:INBOX:3"));
    }

    #[test]
    fn hybrid_search_lets_a_strong_semantic_match_reorder_within_the_keyword_results() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "budget alpha", 3000)).unwrap();
        db.upsert_message(&msg(2, "budget beta", 2000)).unwrap();
        db.upsert_message(&msg(3, "budget gamma", 1000)).unwrap();

        let baseline = db.search_messages("acc1", "budget", 50).unwrap();
        assert_eq!(baseline.len(), 3);
        let top_id = baseline[0].id.clone();
        let mid_id = baseline[1].id.clone();
        let weak_id = baseline[2].id.clone();

        // The keyword-weakest message gets a near-perfect semantic match to
        // the query; the middle one is never embedded at all.
        let query_vector = vec![1.0f32, 0.0, 0.0];
        db.upsert_message_embedding(
            &weak_id,
            "nomic-embed-text",
            3,
            &embed_le(&[0.99, 0.01, 0.0]),
        )
        .unwrap();

        let hybrid = db
            .search_messages_hybrid(Some("acc1"), "budget", 50, Some(&query_vector))
            .unwrap();
        assert_eq!(hybrid.len(), 3, "keyword results stay complete");
        let pos = |id: &str| hybrid.iter().position(|h| h.id == id).unwrap();

        // combine_scores' documented weighting: rank 0 with no embedding
        // (0.7) still beats rank 2 with a near-1.0 similarity (~0.53), but
        // that same similarity comfortably beats rank 1 with no embedding
        // at all (0.35) — see search_query::combine_scores' doc comment.
        assert!(
            pos(&top_id) < pos(&weak_id),
            "top keyword result is not displaced by a semantic match elsewhere"
        );
        assert!(
            pos(&weak_id) < pos(&mid_id),
            "a strong semantic match pulls the weakest keyword result ahead of an un-embedded one"
        );
    }

    // -- find_similar_messages (S53-UI) --------------------------------------

    #[test]
    fn find_similar_messages_sorts_by_cosine_similarity_descending() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "anchor", 3000)).unwrap();
        db.upsert_message(&msg(2, "close match", 2000)).unwrap();
        db.upsert_message(&msg(3, "far match", 1000)).unwrap();

        let query_vector = vec![1.0f32, 0.0, 0.0];
        // #2: near-identical to the query. #3: nearly orthogonal.
        db.upsert_message_embedding("acc1:INBOX:2", "m", 3, &embed_le(&[0.99, 0.01, 0.0]))
            .unwrap();
        db.upsert_message_embedding("acc1:INBOX:3", "m", 3, &embed_le(&[0.1, 0.99, 0.0]))
            .unwrap();

        let results = db
            .find_similar_messages(Some("acc1"), &query_vector, "acc1:INBOX:1", 50)
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0.id, "acc1:INBOX:2", "closer match sorts first");
        assert_eq!(results[1].0.id, "acc1:INBOX:3");
        assert!(
            results[0].1 > results[1].1,
            "similarity scores are descending"
        );
    }

    #[test]
    fn find_similar_messages_never_includes_the_excluded_anchor() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "anchor", 2000)).unwrap();
        db.upsert_message(&msg(2, "other", 1000)).unwrap();

        let query_vector = vec![1.0f32, 0.0, 0.0];
        // The anchor itself has a stored embedding (it was embedded on
        // ingest, same as every message) — it must never appear in its own
        // results even though it would otherwise be the closest match.
        db.upsert_message_embedding("acc1:INBOX:1", "m", 3, &embed_le(&[1.0, 0.0, 0.0]))
            .unwrap();
        db.upsert_message_embedding("acc1:INBOX:2", "m", 3, &embed_le(&[0.5, 0.5, 0.0]))
            .unwrap();

        let results = db
            .find_similar_messages(Some("acc1"), &query_vector, "acc1:INBOX:1", 50)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.id, "acc1:INBOX:2");
    }

    #[test]
    fn find_similar_messages_omits_candidates_with_no_stored_embedding() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "anchor", 3000)).unwrap();
        db.upsert_message(&msg(2, "embedded", 2000)).unwrap();
        db.upsert_message(&msg(3, "never embedded", 1000)).unwrap();

        let query_vector = vec![1.0f32, 0.0, 0.0];
        db.upsert_message_embedding("acc1:INBOX:2", "m", 3, &embed_le(&[1.0, 0.0, 0.0]))
            .unwrap();

        let results = db
            .find_similar_messages(Some("acc1"), &query_vector, "acc1:INBOX:1", 50)
            .unwrap();
        assert_eq!(
            results.len(),
            1,
            "a candidate with no message_embeddings row is silently absent, never an error"
        );
        assert_eq!(results[0].0.id, "acc1:INBOX:2");
    }

    #[test]
    fn find_similar_messages_honours_limit() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "anchor", 4000)).unwrap();
        for (n, v) in [(2, 0.9f32), (3, 0.8), (4, 0.7)] {
            db.upsert_message(&msg(n, "candidate", 4000 - n)).unwrap();
            db.upsert_message_embedding(
                &format!("acc1:INBOX:{n}"),
                "m",
                3,
                &embed_le(&[v, 0.0, 0.0]),
            )
            .unwrap();
        }

        let query_vector = vec![1.0f32, 0.0, 0.0];
        let results = db
            .find_similar_messages(Some("acc1"), &query_vector, "acc1:INBOX:1", 2)
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0.id, "acc1:INBOX:2");
        assert_eq!(results[1].0.id, "acc1:INBOX:3");
    }

    #[test]
    fn find_similar_messages_on_an_empty_embeddings_table_returns_empty() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "anchor", 2000)).unwrap();
        db.upsert_message(&msg(2, "other", 1000)).unwrap();

        let query_vector = vec![1.0f32, 0.0, 0.0];
        let results = db
            .find_similar_messages(Some("acc1"), &query_vector, "acc1:INBOX:1", 50)
            .unwrap();
        assert!(
            results.is_empty(),
            "a fresh profile with nothing embedded yet returns empty, not an error"
        );
    }

    #[test]
    fn find_similar_messages_scoped_to_none_searches_every_account() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_second_account_mailbox(&db, "acc2", "inbox");
        db.upsert_message(&msg(1, "anchor", 3000)).unwrap();
        db.upsert_message(&msg_for("acc2", "acc2:BOX", 1, "cross-account", 2000))
            .unwrap();

        let query_vector = vec![1.0f32, 0.0, 0.0];
        db.upsert_message_embedding("acc2:BOX:1", "m", 3, &embed_le(&[1.0, 0.0, 0.0]))
            .unwrap();

        let results = db
            .find_similar_messages(None, &query_vector, "acc1:INBOX:1", 50)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.account_id, "acc2");
    }

    #[test]
    fn evaluation_mode_guards_the_enqueue_layer() {
        let db = fresh_db();
        seed_account_mailbox(&db); // writable
        db.upsert_message(&msg(7, "Subject", 1_800_000_000))
            .unwrap();
        db.set_account_write_enabled("acc1", false).unwrap(); // evaluation mode

        // Auto-mark-read (opening a message): LOCAL flag updates, but NO
        // remote intent is queued. Reading mail must not require leaving
        // read-only, so this is the one write that is never refused.
        db.mark_read_auto("acc1:INBOX:7", true).unwrap();
        assert!(
            db.pending_operations("acc1").unwrap().is_empty(),
            "auto-mark-read must not queue remote intent in evaluation mode"
        );
        assert!(
            db.get_message_detail("acc1:INBOX:7")
                .unwrap()
                .unwrap()
                .header
                .is_read,
            "local read flag still updates optimistically"
        );

        // Every explicit mutation is refused with the safe eval-mode error —
        // including an explicit Toggle Read/Unread, which is a server \Seen
        // change like any other write and must not silently "succeed" only to
        // be reverted by the next sync.
        for res in [
            db.mark_read("acc1:INBOX:7", false),
            db.set_flagged("acc1:INBOX:7", true),
            db.archive_message("acc1:INBOX:7"),
            db.delete_message("acc1:INBOX:7"),
            db.mark_all_read("acc1", "acc1:INBOX").map(|_| ()),
            db.empty_trash("acc1").map(|_| ()),
        ] {
            let err = res.expect_err("mutation must be refused in evaluation mode");
            assert!(err.to_string().contains("evaluation mode"), "got {err}");
        }
        // Nothing was queued and the message still exists locally.
        assert!(db.pending_operations("acc1").unwrap().is_empty());
        assert!(db.get_message_detail("acc1:INBOX:7").unwrap().is_some());
        // The refused toggle changed nothing at all — not even locally, so the
        // list cannot show a state the server does not have.
        assert!(
            db.get_message_detail("acc1:INBOX:7")
                .unwrap()
                .unwrap()
                .header
                .is_read,
            "a refused explicit toggle must leave the local flag untouched"
        );
    }

    #[test]
    fn compact_uid_ranges_collapses_runs_and_leaves_singles() {
        assert_eq!(compact_uid_ranges(&[]), "");
        assert_eq!(compact_uid_ranges(&[3]), "3");
        assert_eq!(compact_uid_ranges(&[3, 5, 6, 7, 10]), "3,5:7,10");
        assert_eq!(compact_uid_ranges(&[1, 2, 3, 4]), "1:4");
    }

    #[test]
    fn empty_trash_snapshots_uidvalidity_and_exactly_the_uid_rows() {
        // The write-safety fix (readiness review): the payload must carry the
        // mailbox's UIDVALIDITY plus exactly the provider UIDs of the local
        // Trash rows at THIS moment, not a bare path replay expunges "by
        // whatever is in Trash when it happens to run".
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Trash".into(),
            account_id: "acc1".into(),
            path: "Trash".into(),
            name: "Trash".into(),
            role: Some("trash".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.set_sync_cursor(
            "acc1",
            "acc1:Trash",
            &SyncCursor {
                uid_validity: Some(42),
                uid_next: Some(20),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
        // Three server-known rows (uid 5, 6, 8 — 5:6 collapses to a range) plus
        // one local-only row (no provider_uid, e.g. a queued-but-unsynced
        // state) that must be dropped locally but never appear in the UID set.
        for (uid, rfc) in [(5, "<5@x>"), (6, "<6@x>"), (8, "<8@x>")] {
            let mut m = msg(uid, "t", 1000);
            m.id = format!("acc1:Trash:{uid}");
            m.mailbox_id = "acc1:Trash".into();
            m.rfc822_message_id = Some(rfc.into());
            db.upsert_message(&m).unwrap();
        }
        let mut local_only = msg(999, "local draft-ish", 1000);
        local_only.id = "acc1:Trash:local1".into();
        local_only.mailbox_id = "acc1:Trash".into();
        local_only.provider_uid = None;
        local_only.rfc822_message_id = None;
        db.upsert_message(&local_only).unwrap();

        let removed = db.empty_trash("acc1").unwrap();
        assert_eq!(removed, 4, "all four local Trash rows are dropped");

        let op = db
            .pending_operations("acc1")
            .unwrap()
            .into_iter()
            .find(|o| o.kind == "empty_trash")
            .expect("empty_trash op queued");
        let payload: serde_json::Value = serde_json::from_str(&op.payload_json).unwrap();
        assert_eq!(payload["path"], "Trash");
        assert_eq!(payload["uid_validity"], 42);
        assert_eq!(
            payload["uids"], "5:6,8",
            "local-only row never enters the snapshot"
        );
    }

    #[test]
    fn empty_trash_with_no_sync_cursor_snapshots_uidvalidity_zero() {
        // A Trash mailbox nobody has synced yet (fresh local role fallback)
        // has no sync_cursors row. 0 is not a value any real server reports
        // (RFC 3501 nz-number), so it can never accidentally match a live
        // UIDVALIDITY at replay time — the mismatch check fails closed.
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Trash".into(),
            account_id: "acc1".into(),
            path: "Trash".into(),
            name: "Trash".into(),
            role: Some("trash".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.empty_trash("acc1").unwrap();
        let op = db
            .pending_operations("acc1")
            .unwrap()
            .into_iter()
            .find(|o| o.kind == "empty_trash")
            .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&op.payload_json).unwrap();
        assert_eq!(payload["uid_validity"], 0);
        assert_eq!(payload["uids"], "");
    }

    /// PR #165 review point 1: a message the user just deleted (queuing a
    /// `move` to Trash) has NOT reached Trash yet — `enqueue_move` deletes
    /// the source row immediately but only the replayed op actually moves it
    /// server-side. Empty Trash must still get rid of it: the pending move
    /// is rewritten into a `delete_permanent` targeting the SOURCE UID.
    #[test]
    fn empty_trash_rewrites_a_pending_move_to_trash_into_a_permanent_delete_on_the_source() {
        let db = fresh_db();
        seed_account_mailbox(&db); // acc1:INBOX
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Trash".into(),
            account_id: "acc1".into(),
            path: "Trash".into(),
            name: "Trash".into(),
            role: Some("trash".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.set_sync_cursor(
            "acc1",
            "acc1:INBOX",
            &SyncCursor {
                uid_validity: Some(11),
                uid_next: Some(8),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
        db.upsert_message(&msg(7, "to be deleted", 1000)).unwrap();
        db.delete_message("acc1:INBOX:7").unwrap(); // queues a `move` to Trash

        let before = db.pending_operations("acc1").unwrap();
        assert_eq!(
            before.iter().filter(|o| o.kind == "move").count(),
            1,
            "the move is queued before Empty Trash runs"
        );

        db.empty_trash("acc1").unwrap();

        let after = db.pending_operations("acc1").unwrap();
        assert!(
            after.iter().all(|o| o.kind != "move"),
            "the pending move must be rewritten, not left pointing at a message that never reached Trash: {after:?}"
        );
        let dp = after
            .iter()
            .find(|o| o.kind == "delete_permanent")
            .expect("a delete_permanent op on the source message");
        assert_eq!(dp.status, "queued");
        assert_eq!(dp.attempts, 0);
        let payload: serde_json::Value = serde_json::from_str(&dp.payload_json).unwrap();
        assert_eq!(payload["path"], "INBOX", "targets the SOURCE mailbox");
        assert_eq!(payload["uid_validity"], 11);
        assert_eq!(payload["uids"], "7");
    }

    /// Round-2 review HOLE: the rewrite must copy the UIDVALIDITY that was
    /// current when the move was QUEUED, never re-read it fresh at Empty
    /// Trash time. Scenario: an interrupted first import leaves rows under
    /// V1 with no `sync_cursors` row at all (the cursor is written once, at
    /// the END of a sync pass — see `sync.rs`). The user deletes one such
    /// message (move queued with NO cursor to record). The server then
    /// resets to V2; the next sync's first-import branch (`cursor.is_none()`)
    /// never runs the reset-detection/purge path, so it just writes cursor
    /// V2 without cancelling the pending move. If Empty Trash re-read the
    /// CURRENT cursor at rewrite time, it would stamp V2 onto a UID captured
    /// under V1 — and if some unrelated message now occupies that UID under
    /// V2, replay would permanently delete THAT message, not the one the
    /// user actually deleted. Recording the UIDVALIDITY in the `move`
    /// payload at enqueue time and copying it verbatim closes this: a move
    /// enqueued with no cursor carries `0`, which `expunge_snapshot` refuses
    /// (fails closed) regardless of what the cursor says later.
    #[test]
    fn empty_trash_copies_the_moves_own_enqueue_time_uidvalidity_not_a_later_cursor() {
        let db = fresh_db();
        seed_account_mailbox(&db); // acc1:INBOX, no sync cursor yet
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Trash".into(),
            account_id: "acc1".into(),
            path: "Trash".into(),
            name: "Trash".into(),
            role: Some("trash".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.upsert_message(&msg(7, "orphaned by an interrupted import", 1000))
            .unwrap();
        db.delete_message("acc1:INBOX:7").unwrap(); // move queued — NO cursor exists

        // The next sync completes and writes a cursor — a UIDVALIDITY RESET
        // (V2), simulating the server having reset between the interrupted
        // first import and this sync.
        db.set_sync_cursor(
            "acc1",
            "acc1:INBOX",
            &SyncCursor {
                uid_validity: Some(99),
                uid_next: Some(3),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();

        db.empty_trash("acc1").unwrap();

        let dp = db
            .pending_operations("acc1")
            .unwrap()
            .into_iter()
            .find(|o| o.kind == "delete_permanent")
            .expect("the move was rewritten");
        let payload: serde_json::Value = serde_json::from_str(&dp.payload_json).unwrap();
        assert_eq!(
            payload["uid_validity"], 0,
            "must carry the UIDVALIDITY recorded when the move was queued (none existed), \
             never the cursor written by a LATER sync"
        );
    }

    /// The documented residual: a move already `running` (potentially
    /// mid-flight on the wire) is left untouched by the rewrite — Empty
    /// Trash does not race the executor's own status update.
    #[test]
    fn empty_trash_leaves_a_running_move_to_trash_untouched() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Trash".into(),
            account_id: "acc1".into(),
            path: "Trash".into(),
            name: "Trash".into(),
            role: Some("trash".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.upsert_message(&msg(7, "in flight", 1000)).unwrap();
        db.delete_message("acc1:INBOX:7").unwrap();
        let move_op = db
            .pending_operations("acc1")
            .unwrap()
            .into_iter()
            .find(|o| o.kind == "move")
            .unwrap();
        db.set_operation_status(move_op.id, "running", None)
            .unwrap();

        db.empty_trash("acc1").unwrap();

        let after = db.pending_operations("acc1").unwrap();
        let still_move = after
            .iter()
            .find(|o| o.id == move_op.id)
            .expect("the running move op still exists, unrewritten");
        assert_eq!(still_move.kind, "move");
        assert_eq!(still_move.status, "running");
        assert!(
            after.iter().all(|o| o.kind != "delete_permanent"),
            "a running move must not be rewritten"
        );
    }

    /// Round-2 review point 3: `clear_sync_cursor` must also purge the
    /// mailbox's local rows, in the SAME transaction as clearing the cursor
    /// — exactly like the sync engine's own UIDVALIDITY-reset path
    /// (`delete_messages_for_mailbox` alongside the cursor rewrite). Without
    /// this, a legacy `empty_trash` op's recovery leaves stale local rows
    /// (their UIDs meaningful only under the OLD UID space) sitting under a
    /// cleared cursor; if the server's UIDVALIDITY changes again before the
    /// next sync runs, those rows would look current under the new cursor,
    /// and a later Empty Trash could expunge them in a UID space they never
    /// belonged to.
    #[test]
    fn clear_sync_cursor_also_purges_the_mailboxs_local_rows() {
        let db = fresh_db();
        seed_account_mailbox(&db); // acc1:INBOX
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Trash".into(),
            account_id: "acc1".into(),
            path: "Trash".into(),
            name: "Trash".into(),
            role: Some("trash".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.set_sync_cursor(
            "acc1",
            "acc1:Trash",
            &SyncCursor {
                uid_validity: Some(5),
                uid_next: Some(3),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
        let mut stale = msg(1, "stale under the old UID space", 1000);
        stale.id = "acc1:Trash:1".into();
        stale.mailbox_id = "acc1:Trash".into();
        stale.rfc822_message_id = Some("<stale@x>".into());
        db.upsert_message(&stale).unwrap();
        // An unrelated mailbox's rows/cursor must survive untouched.
        db.upsert_message(&msg(9, "unrelated", 1000)).unwrap();

        db.clear_sync_cursor("acc1", "Trash").unwrap();

        assert!(
            db.get_sync_cursor("acc1", "acc1:Trash").unwrap().is_none(),
            "cursor cleared"
        );
        let remaining = db.count_messages("acc1", "acc1:Trash").unwrap();
        assert_eq!(
            remaining, 0,
            "stale Trash rows must be purged, not just the cursor"
        );
        assert_eq!(
            db.count_messages("acc1", "acc1:INBOX").unwrap(),
            1,
            "an unrelated mailbox's rows survive"
        );
    }

    fn msg_from(uid: i64, subject: &str, from_addr: &str, sent_at: i64) -> NewMessage {
        let mut m = msg(uid, subject, sent_at);
        m.from_address = Some(from_addr.into());
        m.addresses = vec![Address {
            kind: "from".into(),
            name: None,
            address: from_addr.into(),
        }];
        m
    }

    #[test]
    fn from_filter_matches_the_from_address_only() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg_from(1, "Hi", "alice@x.test", 1000))
            .unwrap();
        db.upsert_message(&msg_from(2, "Hi", "bob@x.test", 2000))
            .unwrap();
        let hits = db.search_messages("acc1", "from:alice", 50).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].from_address.as_deref(), Some("alice@x.test"));
    }

    #[test]
    fn subject_filter_uses_like_substring_matching() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "Weekly report", 1000)).unwrap();
        db.upsert_message(&msg(2, "Lunch plans", 2000)).unwrap();
        assert_eq!(
            db.search_messages("acc1", "subject:report", 50)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.search_messages("acc1", "subject:nope", 50)
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn has_attachment_filter() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let mut with_attachment = msg(1, "Has one", 1000);
        with_attachment.has_attachments = true;
        db.upsert_message(&with_attachment).unwrap();
        db.upsert_message(&msg(2, "Has none", 2000)).unwrap();
        let hits = db.search_messages("acc1", "has:attachment", 50).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].subject.as_deref(), Some("Has one"));
    }

    #[test]
    fn before_after_filter_the_sent_at_range() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        // 2026-09-18 / -19 / -20 00:00 UTC (verified against Python's
        // datetime.datetime(2026, 9, D, tzinfo=timezone.utc).timestamp()) —
        // the plan's literals here were computed for 2025, not 2026; fixed
        // as a deviation, see the plan's Deviations list.
        db.upsert_message(&msg(1, "Day 18", 1_789_689_600)).unwrap(); // 2026-09-18 00:00 UTC
        db.upsert_message(&msg(2, "Day 19", 1_789_776_000)).unwrap(); // 2026-09-19 00:00 UTC
        db.upsert_message(&msg(3, "Day 20", 1_789_862_400)).unwrap(); // 2026-09-20 00:00 UTC
        let hits = db
            .search_messages("acc1", "after:2026-09-18 before:2026-09-20", 50)
            .unwrap();
        let subjects: Vec<&str> = hits.iter().filter_map(|h| h.subject.as_deref()).collect();
        assert_eq!(
            subjects,
            vec!["Day 19", "Day 18"],
            "after: is inclusive, before: exclusive, newest first"
        );
    }

    #[test]
    fn in_filter_matches_by_mailbox_role() {
        let db = fresh_db();
        seed_account_mailbox(&db); // acc1:INBOX role "inbox"
        db.upsert_mailbox(&Mailbox {
            id: "acc1:TRASH".into(),
            account_id: "acc1".into(),
            path: "Trash".into(),
            name: "Trash".into(),
            role: Some("trash".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.upsert_message(&msg(1, "Inbox mail", 1000)).unwrap();
        let mut trashed = msg(2, "Trashed mail", 2000);
        trashed.id = "acc1:TRASH:2".into();
        trashed.mailbox_id = "acc1:TRASH".into();
        db.upsert_message(&trashed).unwrap();

        assert_eq!(db.search_messages("acc1", "in:trash", 50).unwrap().len(), 1);
        assert_eq!(db.search_messages("acc1", "in:inbox", 50).unwrap().len(), 1);
        assert_eq!(
            db.search_messages("acc1", "in:archive", 50).unwrap().len(),
            0,
            "unknown role matches nothing, not everything"
        );
    }

    #[test]
    fn is_read_unread_flagged_filter_message_flags() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "Read one", 1000)).unwrap();
        db.upsert_message(&msg(2, "Unread one", 2000)).unwrap();
        db.mark_read("acc1:INBOX:1", true).unwrap();
        db.set_flagged("acc1:INBOX:2", true).unwrap();

        assert_eq!(db.search_messages("acc1", "is:read", 50).unwrap().len(), 1);
        assert_eq!(
            db.search_messages("acc1", "is:unread", 50).unwrap().len(),
            1
        );
        assert_eq!(
            db.search_messages("acc1", "is:flagged", 50).unwrap().len(),
            1
        );
        assert_eq!(
            db.search_messages("acc1", "is:starred", 50).unwrap().len(),
            1,
            "flagged and starred are synonyms"
        );
    }

    #[test]
    fn negation_excludes_matches() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg_from(1, "From Alice", "alice@x.test", 1000))
            .unwrap();
        db.upsert_message(&msg_from(2, "From Bob", "bob@x.test", 2000))
            .unwrap();
        let hits = db.search_messages("acc1", "-from:alice", 50).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].from_address.as_deref(), Some("bob@x.test"));
    }

    #[test]
    fn two_negated_terms_exclude_a_message_carrying_either() {
        // FTS5 ANDs adjacent phrases, so joining both negated terms into one
        // MATCH string would exclude only the message carrying BOTH words.
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg_from(1, "Cheap spam offer", "alice@x.test", 1000))
            .unwrap();
        db.upsert_message(&msg_from(2, "Some junk mail", "bob@x.test", 2000))
            .unwrap();
        db.upsert_message(&msg_from(3, "Quarterly budget", "carol@x.test", 3000))
            .unwrap();
        let hits = db.search_messages("acc1", "-spam -junk", 50).unwrap();
        assert_eq!(hits.len(), 1, "both the spam and the junk mail must go");
        assert_eq!(hits[0].from_address.as_deref(), Some("carol@x.test"));
    }

    #[test]
    fn combined_operators_and_free_text_all_apply() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let mut match_all = msg_from(1, "Quarterly budget", "alice@x.test", 1000);
        match_all.has_attachments = true;
        db.upsert_message(&match_all).unwrap();
        // Same sender/subject text but no attachment - must be excluded.
        db.upsert_message(&msg_from(2, "Quarterly budget", "alice@x.test", 2000))
            .unwrap();
        let hits = db
            .search_messages("acc1", "budget from:alice has:attachment", 50)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].has_attachments);
    }

    #[test]
    fn quoted_phrase_and_negated_free_text_across_accounts() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_second_account_mailbox(&db, "acc2", "inbox");
        db.upsert_message(&msg(1, "weekly report", 1000)).unwrap();
        db.upsert_message(&msg_for(
            "acc2",
            "acc2:BOX",
            1,
            "spam weekly nonsense",
            2000,
        ))
        .unwrap();
        let hits = db
            .search_messages_across_accounts("\"weekly report\" -spam", 50)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].account_id, "acc1");
    }

    #[test]
    fn hostile_query_strings_never_error_through_the_full_pipeline() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "Budget", 1000)).unwrap();
        for q in [
            "",
            "   ",
            "-",
            "::::",
            "\"unterminated",
            "before:garbage after:9999999999999999",
            "has:nonsense is:bogus in:",
            "from: to: cc: subject:",
            "café 日本語 -\"quöted\"",
        ] {
            db.search_messages("acc1", q, 10)
                .unwrap_or_else(|e| panic!("query {q:?} errored: {e}"));
            db.search_messages_across_accounts(q, 10)
                .unwrap_or_else(|e| panic!("query {q:?} errored: {e}"));
        }
    }

    // ---- Undo for archive / delete ----

    fn seed_role_mailbox(db: &Database, path: &str, role: &str) {
        db.upsert_mailbox(&Mailbox {
            id: format!("acc1:{path}"),
            account_id: "acc1".into(),
            path: path.into(),
            name: path.into(),
            role: Some(role.into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
    }

    /// An account with INBOX/Trash/Archive and `n` Inbox messages 1..=n.
    fn undo_db(n: i64) -> Database {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_role_mailbox(&db, "Trash", "trash");
        seed_role_mailbox(&db, "Archive", "archive");
        for uid in 1..=n {
            db.upsert_message(&msg(uid, "s", 1000 + uid)).unwrap();
        }
        db
    }

    fn op_payload(db: &Database, kind: &str, nth: usize) -> serde_json::Value {
        let op = db
            .pending_operations("acc1")
            .unwrap()
            .into_iter()
            .filter(|o| o.kind == kind)
            .nth(nth)
            .unwrap_or_else(|| panic!("no {kind} op #{nth}"));
        serde_json::from_str(&op.payload_json).unwrap()
    }

    #[test]
    fn move_messages_returns_one_op_id_per_message() {
        let db = undo_db(3);
        let ids: Vec<String> = (1..=3).map(|u| format!("acc1:INBOX:{u}")).collect();
        let out = db.move_messages(&ids, MoveTarget::Archive).unwrap();
        assert_eq!(out.operation_ids.len(), 3);
        assert!(out.refused.is_empty());
        let queued: Vec<i64> = db
            .pending_operations("acc1")
            .unwrap()
            .iter()
            .filter(|o| o.kind == "move")
            .map(|o| o.id)
            .collect();
        assert_eq!(
            queued, out.operation_ids,
            "ids are the move ops, in input order"
        );
    }

    #[test]
    fn move_messages_refusals_do_not_stop_the_batch() {
        let db = undo_db(2);
        let ids = vec![
            "acc1:INBOX:1".to_string(),
            "acc1:INBOX:nope".to_string(),
            "acc1:INBOX:2".to_string(),
        ];
        let out = db.move_messages(&ids, MoveTarget::Trash).unwrap();
        assert_eq!(out.operation_ids.len(), 2);
        assert_eq!(out.refused.len(), 1);
        assert_eq!(out.refused[0].message_id, "acc1:INBOX:nope");
        assert!(!out.refused[0].evaluation_mode);
    }

    #[test]
    fn move_messages_flags_evaluation_mode_refusals() {
        let db = undo_db(1);
        db.set_account_write_enabled("acc1", false).unwrap();
        let out = db
            .move_messages(&["acc1:INBOX:1".to_string()], MoveTarget::Archive)
            .unwrap();
        assert!(out.operation_ids.is_empty());
        assert!(out.refused[0].evaluation_mode);
    }

    #[test]
    fn move_messages_mailbox_target_rejects_other_account() {
        let db = undo_db(1);
        let out = db
            .move_messages(
                &["acc1:INBOX:1".to_string()],
                MoveTarget::Mailbox("acc2:Elsewhere"),
            )
            .unwrap();
        assert!(out.operation_ids.is_empty());
        assert_eq!(out.refused.len(), 1);
        assert!(db.get_message_detail("acc1:INBOX:1").unwrap().is_some());
    }

    #[test]
    fn undo_moves_queues_inverse_and_marks_original_undone() {
        let db = undo_db(1);
        let out = db
            .move_messages(&["acc1:INBOX:1".to_string()], MoveTarget::Archive)
            .unwrap();
        let undo = db.undo_moves(&out.operation_ids).unwrap();
        assert_eq!(undo.queued, 1);
        assert!(undo.not_undoable.is_empty());
        assert_eq!(undo.account_ids, vec!["acc1".to_string()]);
        assert_eq!(undo.source_mailbox_ids, vec!["acc1:INBOX".to_string()]);

        let original = op_payload(&db, "move", 0);
        assert_eq!(original["undone"], true);
        let inverse = op_payload(&db, "undo_move", 0);
        assert_eq!(inverse["src_path"], "Archive", "roles swapped");
        assert_eq!(inverse["dest_path"], "INBOX");
        assert_eq!(inverse["message_id"], "<1@local.test>");
        assert_eq!(inverse["undo_of"], out.operation_ids[0]);
        assert_eq!(inverse["uid"], 1, "the ORIGINAL source uid");
    }

    #[test]
    fn undo_moves_twice_is_refused() {
        let db = undo_db(1);
        let out = db
            .move_messages(&["acc1:INBOX:1".to_string()], MoveTarget::Trash)
            .unwrap();
        db.undo_moves(&out.operation_ids).unwrap();
        let again = db.undo_moves(&out.operation_ids).unwrap();
        assert_eq!(again.queued, 0);
        assert_eq!(again.not_undoable.len(), 1);
        assert_eq!(
            db.pending_operations("acc1")
                .unwrap()
                .iter()
                .filter(|o| o.kind == "undo_move")
                .count(),
            1
        );
    }

    #[test]
    fn undo_moves_refused_in_evaluation_mode() {
        let db = undo_db(1);
        let out = db
            .move_messages(&["acc1:INBOX:1".to_string()], MoveTarget::Archive)
            .unwrap();
        db.set_account_write_enabled("acc1", false).unwrap();
        let undo = db.undo_moves(&out.operation_ids).unwrap();
        assert_eq!(undo.queued, 0);
        assert!(undo.not_undoable[0].evaluation_mode);
        assert!(undo.not_undoable[0].reason.contains("evaluation mode"));
    }

    #[test]
    fn undo_moves_refuses_unknown_and_non_move_ops() {
        let db = undo_db(1);
        db.mark_read("acc1:INBOX:1", true).unwrap();
        let read_op = db.pending_operations("acc1").unwrap()[0].id;
        let undo = db.undo_moves(&[read_op, 9_999_999]).unwrap();
        assert_eq!(undo.queued, 0);
        assert_eq!(undo.not_undoable.len(), 2);
    }

    #[test]
    fn undo_moves_refuses_op_rewritten_by_empty_trash() {
        let db = undo_db(1);
        let out = db
            .move_messages(&["acc1:INBOX:1".to_string()], MoveTarget::Trash)
            .unwrap();
        db.empty_trash("acc1").unwrap(); // rewrites the pending move into delete_permanent
        let undo = db.undo_moves(&out.operation_ids).unwrap();
        assert_eq!(undo.queued, 0);
        assert_eq!(undo.not_undoable.len(), 1);
    }

    #[test]
    fn undo_moves_drops_local_destination_row() {
        let db = undo_db(0);
        // The message already synced into Trash (as after a replayed delete).
        let mut in_trash = msg(40, "back", 5);
        in_trash.id = "acc1:Trash:40".into();
        in_trash.mailbox_id = "acc1:Trash".into();
        in_trash.rfc822_message_id = Some("<1@local.test>".into());
        db.upsert_message(&in_trash).unwrap();
        db.upsert_message(&msg(1, "s", 1)).unwrap();
        let out = db
            .move_messages(&["acc1:INBOX:1".to_string()], MoveTarget::Trash)
            .unwrap();
        db.undo_moves(&out.operation_ids).unwrap();
        assert!(
            db.get_message_detail("acc1:Trash:40").unwrap().is_none(),
            "the Trash copy leaves the Trash view at once"
        );
    }

    #[test]
    fn empty_trash_skips_moves_being_undone() {
        let db = undo_db(1);
        let out = db
            .move_messages(&["acc1:INBOX:1".to_string()], MoveTarget::Trash)
            .unwrap();
        db.undo_moves(&out.operation_ids).unwrap();
        db.empty_trash("acc1").unwrap();
        let ops = db.pending_operations("acc1").unwrap();
        assert!(ops.iter().all(|o| o.kind != "delete_permanent"), "{ops:?}");
        assert!(ops.iter().any(|o| o.kind == "move"), "move still queued");
    }

    #[test]
    fn empty_trash_snapshot_excludes_message_being_undone() {
        let db = undo_db(0);
        let mut in_trash = msg(40, "back", 5);
        in_trash.id = "acc1:Trash:40".into();
        in_trash.mailbox_id = "acc1:Trash".into();
        in_trash.rfc822_message_id = Some("<9@local.test>".into());
        db.upsert_message(&in_trash).unwrap();
        let mut other = msg(41, "other", 6);
        other.id = "acc1:Trash:41".into();
        other.mailbox_id = "acc1:Trash".into();
        other.rfc822_message_id = Some("<10@local.test>".into());
        db.upsert_message(&other).unwrap();
        // A pending undo for <9@local.test> (the sync re-fetched the Trash
        // copy before the undo replayed).
        {
            let conn = db.lock_conn();
            conn.execute(
                "INSERT INTO operations (account_id, kind, payload_json)
                 VALUES ('acc1', 'undo_move', ?1)",
                params![serde_json::json!({
                    "src_path": "Trash", "dest_path": "INBOX", "uid": 9,
                    "message_id": "<9@local.test>", "undo_of": 1
                })
                .to_string()],
            )
            .unwrap();
        }
        db.empty_trash("acc1").unwrap();
        let payload = op_payload(&db, "empty_trash", 0);
        assert_eq!(payload["uids"], "41", "only the other message is expunged");
        assert!(db.get_message_detail("acc1:Trash:40").unwrap().is_some());
        assert!(db.get_message_detail("acc1:Trash:41").unwrap().is_none());
    }

    #[test]
    fn cancel_undo_move_clears_undone_flag() {
        let db = undo_db(1);
        let out = db
            .move_messages(&["acc1:INBOX:1".to_string()], MoveTarget::Archive)
            .unwrap();
        db.undo_moves(&out.operation_ids).unwrap();
        let undo_id = db
            .pending_operations("acc1")
            .unwrap()
            .into_iter()
            .find(|o| o.kind == "undo_move")
            .unwrap()
            .id;
        assert!(db.cancel_operation(undo_id).unwrap());
        assert!(op_payload(&db, "move", 0).get("undone").is_none());
        assert_eq!(
            db.undo_moves(&out.operation_ids).unwrap().queued,
            1,
            "undoable again"
        );
    }

    fn cursor(validity: i64) -> SyncCursor {
        SyncCursor {
            uid_validity: Some(validity),
            uid_next: Some(1000),
            highest_modseq: None,
            descriptor_checkpoint_uid: None,
        }
    }

    /// Priority fix: cancelling the `undo_move` after the original move
    /// reached the server left the message in Archive on the server with its
    /// local row gone (and sync never re-imports below UIDNEXT). The cancel now
    /// queues a re-fetch of the recorded COPYUID.
    #[test]
    fn cancelling_an_undo_after_the_move_succeeded_refetches_the_message() {
        let db = undo_db(1);
        let out = db
            .move_messages(&["acc1:INBOX:1".to_string()], MoveTarget::Archive)
            .unwrap();
        let move_id = out.operation_ids[0];
        db.set_operation_status(move_id, "succeeded", None).unwrap();
        db.record_move_result(move_id, 9, 77).unwrap();
        db.undo_moves(&out.operation_ids).unwrap();
        let undo_id = db
            .pending_operations("acc1")
            .unwrap()
            .into_iter()
            .find(|o| o.kind == "undo_move")
            .unwrap()
            .id;
        assert!(db.cancel_operation(undo_id).unwrap());
        assert_eq!(
            db.pending_refetch_uids("acc1", "Archive", 9, 10).unwrap(),
            vec![77],
            "the Archive copy is fetched back into the local view"
        );
    }

    /// Same cancel, but sync had already brought the Archive copy in and
    /// `undo_moves` dropped it: its UID is remembered and re-fetched too, even
    /// when the server gave no COPYUID.
    #[test]
    fn cancelling_an_undo_refetches_the_local_copy_it_dropped() {
        let db = undo_db(0);
        db.set_sync_cursor("acc1", "acc1:Trash", &cursor(5))
            .unwrap();
        let mut in_trash = msg(40, "back", 5);
        in_trash.id = "acc1:Trash:40".into();
        in_trash.mailbox_id = "acc1:Trash".into();
        in_trash.rfc822_message_id = Some("<1@local.test>".into());
        db.upsert_message(&in_trash).unwrap();
        db.upsert_message(&msg(1, "s", 1)).unwrap();
        let out = db
            .move_messages(&["acc1:INBOX:1".to_string()], MoveTarget::Trash)
            .unwrap();
        db.set_operation_status(out.operation_ids[0], "succeeded", None)
            .unwrap();
        db.undo_moves(&out.operation_ids).unwrap();
        assert!(db.get_message_detail("acc1:Trash:40").unwrap().is_none());
        let undo_id = db
            .pending_operations("acc1")
            .unwrap()
            .into_iter()
            .find(|o| o.kind == "undo_move")
            .unwrap()
            .id;
        db.cancel_operation(undo_id).unwrap();
        assert_eq!(
            db.pending_refetch_uids("acc1", "Trash", 5, 10).unwrap(),
            vec![40]
        );
    }

    /// Review H1: a SUCCESSFUL undo also owes a re-fetch of the destination
    /// copies `undo_moves` dropped. On Gmail the All Mail copy keeps its UID
    /// after the label moves back; a second genuine copy sharing the
    /// Message-ID stays too. UIDs the server no longer holds clear themselves.
    #[test]
    fn a_finished_undo_refetches_the_destination_copies_it_dropped() {
        for terminal in ["succeeded", "failed_permanent"] {
            let db = undo_db(0);
            db.set_sync_cursor("acc1", "acc1:Trash", &cursor(5))
                .unwrap();
            let mut in_trash = msg(40, "back", 5);
            in_trash.id = "acc1:Trash:40".into();
            in_trash.mailbox_id = "acc1:Trash".into();
            in_trash.rfc822_message_id = Some("<1@local.test>".into());
            db.upsert_message(&in_trash).unwrap();
            db.upsert_message(&msg(1, "s", 1)).unwrap();
            let out = db
                .move_messages(&["acc1:INBOX:1".to_string()], MoveTarget::Trash)
                .unwrap();
            db.set_operation_status(out.operation_ids[0], "succeeded", None)
                .unwrap();
            db.undo_moves(&out.operation_ids).unwrap();
            let undo_id = db
                .pending_operations("acc1")
                .unwrap()
                .into_iter()
                .find(|o| o.kind == "undo_move")
                .unwrap()
                .id;
            assert!(db
                .pending_refetch_uids("acc1", "Trash", 5, 10)
                .unwrap()
                .is_empty());
            db.set_operation_status(undo_id, terminal, Some("x"))
                .unwrap();
            assert_eq!(
                db.pending_refetch_uids("acc1", "Trash", 5, 10).unwrap(),
                vec![40],
                "{terminal}"
            );
        }
    }

    /// Review H2: undoing a move that itself failed for good "succeeds" (the
    /// message never left), but the move had dropped the source row. The
    /// finished undo re-fetches the original UID in the source folder.
    #[test]
    fn undoing_a_permanently_failed_move_refetches_the_source_row() {
        let db = undo_db(1);
        db.set_sync_cursor("acc1", "acc1:INBOX", &cursor(3))
            .unwrap();
        let out = db
            .move_messages(&["acc1:INBOX:1".to_string()], MoveTarget::Archive)
            .unwrap();
        let move_id = out.operation_ids[0];
        db.undo_moves(&out.operation_ids).unwrap();
        db.set_operation_status(move_id, "failed_permanent", Some("x"))
            .unwrap();
        let undo_id = db
            .pending_operations("acc1")
            .unwrap()
            .into_iter()
            .find(|o| o.kind == "undo_move")
            .unwrap()
            .id;
        db.set_operation_status(undo_id, "succeeded", None).unwrap();
        assert_eq!(
            db.pending_refetch_uids("acc1", "INBOX", 3, 10).unwrap(),
            vec![1]
        );
    }

    /// Cancelling an undo whose original is still queued needs no re-fetch:
    /// the destination copy has not been created yet.
    #[test]
    fn cancelling_an_undo_before_the_move_ran_queues_no_refetch() {
        let db = undo_db(1);
        let out = db
            .move_messages(&["acc1:INBOX:1".to_string()], MoveTarget::Archive)
            .unwrap();
        db.undo_moves(&out.operation_ids).unwrap();
        let undo_id = db
            .pending_operations("acc1")
            .unwrap()
            .into_iter()
            .find(|o| o.kind == "undo_move")
            .unwrap()
            .id;
        db.cancel_operation(undo_id).unwrap();
        assert!(db
            .pending_refetch_uids("acc1", "Archive", 0, 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn undo_move_for_a_purged_source_folder_is_cancelled_with_its_siblings() {
        let db = undo_db(1);
        let out = db
            .move_messages(&["acc1:INBOX:1".to_string()], MoveTarget::Archive)
            .unwrap();
        db.undo_moves(&out.operation_ids).unwrap();
        // The undo's source is Archive; purging Archive cancels it.
        let cancelled = db
            .cancel_operations_for_purged_mailbox("acc1", "acc1:Archive", "Archive", "gone")
            .unwrap();
        assert!(cancelled >= 1);
    }

    #[test]
    fn expand_uid_ranges_reads_the_compact_form_and_is_capped() {
        assert_eq!(expand_uid_ranges("3,5:7,10", 100), vec![3, 5, 6, 7, 10]);
        assert_eq!(expand_uid_ranges("", 10), Vec::<i64>::new());
        assert_eq!(expand_uid_ranges("x,2:y,4", 10), vec![4], "junk skipped");
        assert_eq!(expand_uid_ranges("1:1000000", 5), vec![1, 2, 3, 4, 5]);
        assert_eq!(
            expand_uid_ranges("9:3", 10),
            Vec::<i64>::new(),
            "backwards range"
        );
    }

    #[test]
    fn cancelling_a_move_or_empty_trash_records_uids_for_refetch() {
        let db = undo_db(2);
        db.set_sync_cursor(
            "acc1",
            "acc1:INBOX",
            &SyncCursor {
                uid_validity: Some(11),
                uid_next: Some(10),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
        db.move_messages(&["acc1:INBOX:1".to_string()], MoveTarget::Archive)
            .unwrap();
        let op = db.pending_operations("acc1").unwrap()[0].id;
        assert!(db.cancel_operation(op).unwrap());
        assert_eq!(
            db.pending_refetch_uids("acc1", "INBOX", 11, 10).unwrap(),
            vec![1]
        );
        // Another UID space: the row is dropped, not returned.
        assert!(db
            .pending_refetch_uids("acc1", "INBOX", 12, 10)
            .unwrap()
            .is_empty());
        assert!(db
            .pending_refetch_uids("acc1", "INBOX", 11, 10)
            .unwrap()
            .is_empty());
        // A cancelled flag op records nothing; neither does a move with no cursor (uid_validity 0).
        db.mark_read("acc1:INBOX:2", true).unwrap();
        let read_op = db.pending_operations("acc1").unwrap()[0].id;
        db.cancel_operation(read_op).unwrap();
        assert!(db
            .pending_refetch_uids("acc1", "INBOX", 11, 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn deferred_rules_wait_for_the_body_expire_after_a_week_and_follow_their_message() {
        let db = undo_db(0);
        for u in 1..=3 {
            let mut m = msg(u, "s", 1000 + u);
            m.text_body = None; // descriptor only
            m.attachments = None;
            db.upsert_message(&m).unwrap();
        }
        let ids: Vec<String> = (1..=3).map(|u| format!("acc1:INBOX:{u}")).collect();
        db.defer_rules_for("acc1", &ids).unwrap();
        db.defer_rules_for("acc1", &ids).unwrap(); // idempotent
        assert_eq!(db.pending_rules_count("acc1").unwrap(), 3);
        assert!(
            db.take_ready_deferred_rules("acc1", 10).unwrap().is_empty(),
            "no bodies yet"
        );
        // Message 1 gets its body; message 2's parked row is a week old.
        let mut hydrated = msg(1, "s", 1001);
        hydrated.text_body = Some("body".into());
        hydrated.attachments = Some(vec![]);
        db.upsert_message(&hydrated).unwrap();
        db.lock_conn()
            .execute(
                "UPDATE pending_rules SET created_at = datetime('now', '-8 days')
                 WHERE message_id = 'acc1:INBOX:2'",
                [],
            )
            .unwrap();
        assert_eq!(
            db.messages_with_bodies(&ids)
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["acc1:INBOX:1".to_string()]
        );
        assert_eq!(
            db.take_ready_deferred_rules("acc1", 10).unwrap(),
            vec!["acc1:INBOX:1".to_string()]
        );
        assert_eq!(
            db.pending_rules_count("acc1").unwrap(),
            1,
            "the week-old row expired, 3 remains"
        );
        // Deleting the message removes its parked row.
        db.lock_conn()
            .execute("DELETE FROM messages WHERE id = 'acc1:INBOX:3'", [])
            .unwrap();
        assert_eq!(db.pending_rules_count("acc1").unwrap(), 0);
    }

    #[test]
    fn clear_pending_refetch_removes_only_the_listed_uids() {
        let db = undo_db(0);
        {
            let conn = db.lock_conn();
            for uid in [1, 2, 3] {
                conn.execute(
                    "INSERT INTO pending_refetch (account_id, mailbox_path, uid, uid_validity)
                     VALUES ('acc1', 'INBOX', ?1, 5)",
                    params![uid],
                )
                .unwrap();
            }
        }
        db.clear_pending_refetch("acc1", "INBOX", 5, &[1, 3])
            .unwrap();
        assert_eq!(
            db.pending_refetch_uids("acc1", "INBOX", 5, 10).unwrap(),
            vec![2]
        );
    }

    fn msg(uid: i64, subject: &str, sent_at: i64) -> NewMessage {
        NewMessage {
            id: format!("acc1:INBOX:{uid}"),
            account_id: "acc1".into(),
            mailbox_id: "acc1:INBOX".into(),
            provider_uid: Some(uid),
            rfc822_message_id: Some(format!("<{uid}@local.test>")),
            subject: Some(subject.into()),
            from_name: Some("Alice".into()),
            from_address: Some("alice@local.test".into()),
            sent_at: Some(sent_at),
            snippet: Some("hello".into()),
            size_bytes: Some(1234),
            has_attachments: false,
            attachments: None,
            is_read: false,
            is_starred: false,
            addresses: vec![
                Address {
                    kind: "from".into(),
                    name: Some("Alice".into()),
                    address: "alice@local.test".into(),
                },
                Address {
                    kind: "to".into(),
                    name: None,
                    address: "test@local.test".into(),
                },
            ],
            text_body: Some("hello world".into()),
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: Vec::new(),
        }
    }

    /// A message with an explicit (bracket-free) message-id and references, for
    /// thread-projection tests.
    fn threaded_msg(
        uid: i64,
        subject: &str,
        sent_at: i64,
        msgid: &str,
        refs: &[&str],
    ) -> NewMessage {
        let mut m = msg(uid, subject, sent_at);
        m.rfc822_message_id = Some(msgid.to_string());
        m.references = refs.iter().map(|s| s.to_string()).collect();
        m
    }

    #[test]
    fn upsert_messages_batch_matches_individual_upserts() {
        //: the batched path must be observably identical to
        // calling upsert_message once per message -- same rows, same flags,
        // same FTS content/ranking, same thread membership.
        let individually = fresh_db();
        seed_account_mailbox(&individually);
        let batched = fresh_db();
        seed_account_mailbox(&batched);

        let messages: Vec<NewMessage> = (1..=250)
            .map(|uid| {
                threaded_msg(
                    uid,
                    &format!("Subject {uid}"),
                    1000 + uid,
                    &format!("<{uid}@x>"),
                    &[],
                )
            })
            .collect();

        for m in &messages {
            individually.upsert_message(m).unwrap();
        }
        batched.upsert_messages_batch(&messages).unwrap();

        assert_eq!(
            individually.count_messages("acc1", "acc1:INBOX").unwrap(),
            batched.count_messages("acc1", "acc1:INBOX").unwrap(),
        );
        assert_eq!(
            individually.count_messages("acc1", "acc1:INBOX").unwrap(),
            250
        );
        for uid in [1, 42, 137, 250] {
            let id = format!("acc1:INBOX:{uid}");
            let a = individually.get_message_detail(&id).unwrap().unwrap();
            let b = batched.get_message_detail(&id).unwrap().unwrap();
            assert_eq!(a.header.subject, b.header.subject);
            assert_eq!(a.header.sent_at, b.header.sent_at);
        }
        assert_eq!(
            individually
                .search_messages("acc1", "Subject", 500)
                .unwrap()
                .len(),
            batched
                .search_messages("acc1", "Subject", 500)
                .unwrap()
                .len(),
        );
    }

    #[test]
    fn upsert_messages_batch_empty_slice_is_a_noop() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_messages_batch(&[]).unwrap();
        assert_eq!(db.count_messages("acc1", "acc1:INBOX").unwrap(), 0);
    }

    #[test]
    fn upsert_messages_batch_is_all_or_nothing() {
        // A batch is ONE transaction: an error partway must
        // roll back every row already applied in this call, not leave a
        // partial import a resume-by-cursor can't reconcile.
        let db = fresh_db();
        seed_account_mailbox(&db);
        let mut messages: Vec<NewMessage> = (1..=5).map(|uid| msg(uid, "ok", 1000 + uid)).collect();
        // A mailbox_id with no matching row violates the FK (foreign_keys=ON)
        // partway through the batch.
        let mut poison = msg(6, "poison", 1006);
        poison.mailbox_id = "acc1:NoSuchMailbox".into();
        messages.push(poison);
        messages.push(msg(7, "never reached", 1007));

        assert!(db.upsert_messages_batch(&messages).is_err());
        assert_eq!(
            db.count_messages("acc1", "acc1:INBOX").unwrap(),
            0,
            "the first 5 good rows must not survive a later row's failure"
        );
    }

    #[test]
    fn accounts_and_mailboxes_round_trip() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        assert_eq!(db.list_accounts().unwrap().len(), 1);
        let mbs = db.list_mailboxes("acc1").unwrap();
        assert_eq!(mbs.len(), 1);
        assert_eq!(mbs[0].path, "INBOX");
    }

    #[test]
    fn message_upsert_is_idempotent_and_lists_newest_first() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "older", 1000)).unwrap();
        db.upsert_message(&msg(2, "newer", 2000)).unwrap();
        // Re-sync the same messages: counts must not grow, addresses not duplicate.
        db.upsert_message(&msg(1, "older-edited", 1000)).unwrap();

        assert_eq!(db.count_messages("acc1", "acc1:INBOX").unwrap(), 2);

        let headers = db
            .list_message_headers("acc1", "acc1:INBOX", 50, 0)
            .unwrap();
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0].subject.as_deref(), Some("newer"));
        assert!(headers[0].has_body);
        assert_eq!(headers[1].subject.as_deref(), Some("older-edited"));
        assert!(headers[1].has_body);

        let detail = db.get_message_detail("acc1:INBOX:1").unwrap().unwrap();
        assert_eq!(
            detail.addresses.len(),
            2,
            "addresses replaced, not duplicated"
        );
        assert_eq!(detail.text_body.as_deref(), Some("hello world"));
        assert!(detail.header.has_body);
        assert!(!detail.header.is_read);
    }

    #[test]
    fn header_reports_descriptor_only_rows_without_bodies() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let mut descriptor = msg(1, "descriptor", 1000);
        descriptor.text_body = None;
        descriptor.html_body = None;
        descriptor.sanitized_html = None;
        db.upsert_message(&descriptor).unwrap();

        let header = db
            .list_message_headers("acc1", "acc1:INBOX", 50, 0)
            .unwrap()
            .remove(0);
        assert!(!header.has_body);

        let detail = db.get_message_detail("acc1:INBOX:1").unwrap().unwrap();
        assert!(!detail.header.has_body);
        assert!(detail.text_body.is_none());
    }

    #[test]
    fn hydration_backlog_counts_only_body_missing_rows_per_mailbox() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Archive".into(),
            account_id: "acc1".into(),
            path: "Archive".into(),
            name: "Archive".into(),
            role: Some("archive".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: false,
        })
        .unwrap();

        // INBOX: one hydrated row, two descriptor-only rows, and an
        // authoritative empty-body marker (must NOT count as missing).
        db.upsert_message(&msg(1, "hydrated", 1000)).unwrap();
        for (uid, sent_at) in [(2, 2000), (3, 3000)] {
            let mut descriptor = msg(uid, "descriptor", sent_at);
            descriptor.text_body = None;
            descriptor.html_body = None;
            descriptor.sanitized_html = None;
            db.upsert_message(&descriptor).unwrap();
        }
        let mut empty_marker = msg(4, "empty marker", 4000);
        empty_marker.text_body = None;
        empty_marker.html_body = None;
        empty_marker.sanitized_html = None;
        empty_marker.attachments = Some(vec![]);
        db.upsert_message(&empty_marker).unwrap();

        // Archive: one descriptor-only row.
        let mut archived = msg(9, "archived descriptor", 500);
        archived.id = "acc1:Archive:9".into();
        archived.mailbox_id = "acc1:Archive".into();
        archived.text_body = None;
        archived.html_body = None;
        archived.sanitized_html = None;
        db.upsert_message(&archived).unwrap();

        let mut backlog = db.list_hydration_backlog("acc1").unwrap();
        backlog.sort_by(|a, b| a.mailbox_id.cmp(&b.mailbox_id));
        assert_eq!(backlog.len(), 2);
        assert_eq!(backlog[0].mailbox_id, "acc1:Archive");
        assert_eq!(backlog[0].path, "Archive");
        assert_eq!(backlog[0].role.as_deref(), Some("archive"));
        assert_eq!(backlog[0].missing, 1);
        assert_eq!(backlog[0].last_activity, 500);
        assert_eq!(backlog[1].mailbox_id, "acc1:INBOX");
        assert_eq!(backlog[1].missing, 2);
        assert_eq!(backlog[1].last_activity, 3000);

        // Hydrating one INBOX row shrinks the backlog without any cursor state.
        db.upsert_message(&msg(3, "now hydrated", 3000)).unwrap();
        let backlog = db.list_hydration_backlog("acc1").unwrap();
        let inbox = backlog
            .iter()
            .find(|b| b.mailbox_id == "acc1:INBOX")
            .unwrap();
        assert_eq!(inbox.missing, 1);
        assert_eq!(inbox.last_activity, 2000);
    }

    #[test]
    fn body_hydration_progress_counts_per_mailbox_including_empty_and_full() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        for (id, path) in [("acc1:Empty", "Empty"), ("acc1:Full", "Full")] {
            db.upsert_mailbox(&Mailbox {
                id: id.into(),
                account_id: "acc1".into(),
                path: path.into(),
                name: path.into(),
                role: None,
                delimiter: Some("/".into()),
                subscribed: true,
                selectable: true,
                sync_by_default: true,
            })
            .unwrap();
        }
        // INBOX: 1 hydrated + 2 descriptor-only + 1 empty-body marker (hydrated).
        db.upsert_message(&msg(1, "hydrated", 1000)).unwrap();
        for (uid, sent_at) in [(2, 2000), (3, 3000)] {
            let mut d = msg(uid, "descriptor", sent_at);
            d.text_body = None;
            d.html_body = None;
            d.sanitized_html = None;
            db.upsert_message(&d).unwrap();
        }
        let mut marker = msg(4, "empty marker", 4000);
        marker.text_body = None;
        marker.html_body = None;
        marker.sanitized_html = None;
        marker.attachments = Some(vec![]);
        db.upsert_message(&marker).unwrap();
        // Full: two hydrated rows.
        for uid in [1, 2] {
            let mut m = msg(uid, "full", 100 * uid);
            m.id = format!("acc1:Full:{uid}");
            m.mailbox_id = "acc1:Full".into();
            db.upsert_message(&m).unwrap();
        }

        let progress = db.body_hydration_progress("acc1").unwrap();
        let get = |id: &str| progress.iter().find(|p| p.mailbox_id == id).unwrap();
        assert_eq!(
            progress.len(),
            3,
            "one row per mailbox, empty ones included"
        );
        assert_eq!(
            (get("acc1:INBOX").total, get("acc1:INBOX").hydrated),
            (4, 2)
        );
        assert_eq!((get("acc1:Full").total, get("acc1:Full").hydrated), (2, 2));
        assert_eq!(
            (get("acc1:Empty").total, get("acc1:Empty").hydrated),
            (0, 0)
        );
        assert!(db.body_hydration_progress("nobody").unwrap().is_empty());

        // Plan check: the per-mailbox total must come from the covering
        // partial index, never a full messages scan.
        let conn = db.lock_conn();
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {BODY_PROGRESS_SQL}"))
            .unwrap()
            .query_map(params!["acc1"], |r| r.get::<_, String>(3))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let plan = plan.join("\n");
        assert!(plan.contains("idx_messages_uid"), "plan: {plan}");
        assert!(
            !plan.contains("SCAN m") && !plan.contains("SCAN messages"),
            "plan: {plan}"
        );
    }

    #[test]
    fn body_hydration_scope_counts_missing_per_scope_and_plans_on_the_index() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_account(&Account {
            id: "acc2".into(),
            email: "two@local.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        for (acct, path) in [("acc1", "Empty"), ("acc2", "INBOX")] {
            db.upsert_mailbox(&Mailbox {
                id: format!("{acct}:{path}"),
                account_id: acct.into(),
                path: path.into(),
                name: path.into(),
                role: None,
                delimiter: Some("/".into()),
                subscribed: true,
                selectable: true,
                sync_by_default: true,
            })
            .unwrap();
        }
        // acc1:INBOX = 1 hydrated + 2 descriptor-only; acc2:INBOX fully hydrated.
        db.upsert_message(&msg(1, "hydrated", 1000)).unwrap();
        for uid in [2, 3] {
            let mut d = msg(uid, "descriptor", 1000 + uid);
            d.text_body = None;
            d.html_body = None;
            d.sanitized_html = None;
            db.upsert_message(&d).unwrap();
        }
        let mut other = msg(1, "other account", 500);
        other.id = "acc2:INBOX:1".into();
        other.account_id = "acc2".into();
        other.mailbox_id = "acc2:INBOX".into();
        db.upsert_message(&other).unwrap();

        let s = |a, m| db.body_hydration_scope(a, m, None).unwrap();
        assert_eq!((s(None, None).total, s(None, None).missing), (4, 2));
        assert_eq!(
            (s(Some("acc1"), None).total, s(Some("acc1"), None).missing),
            (3, 2)
        );
        assert_eq!(
            (s(Some("acc2"), None).total, s(Some("acc2"), None).missing),
            (1, 0)
        );
        let inbox = s(Some("acc1"), Some("acc1:INBOX"));
        assert_eq!((inbox.total, inbox.missing), (3, 2));
        let empty = s(Some("acc1"), Some("acc1:Empty"));
        assert_eq!(
            (empty.total, empty.missing),
            (0, 0),
            "empty mailbox: SUM over no rows is NULL -> 0"
        );
        let none = s(Some("nobody"), None);
        assert_eq!((none.total, none.missing), (0, 0));

        // The searched query's mailbox filters narrow the count (reviewer
        // finding: a search excluding Trash must not be told about Trash).
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Trash".into(),
            account_id: "acc1".into(),
            path: "Trash".into(),
            name: "Trash".into(),
            role: Some("trash".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        let mut trashed = msg(7, "in the bin", 100);
        trashed.id = "acc1:Trash:7".into();
        trashed.mailbox_id = "acc1:Trash".into();
        trashed.text_body = None;
        trashed.html_body = None;
        trashed.sanitized_html = None;
        db.upsert_message(&trashed).unwrap();
        let q = |query| {
            db.body_hydration_scope(Some("acc1"), None, Some(query))
                .unwrap()
        };
        assert_eq!(q("budget").missing, 3, "no mailbox filter: Trash counts");
        assert_eq!(q("budget -in:trash").missing, 2, "-in:trash drops Trash");
        assert_eq!(q("budget in:trash").missing, 1);
        assert_eq!(q("budget -folder:Trash").missing, 2);
        assert_eq!(q("budget path:INBOX").missing, 2);
        assert_eq!(
            q("budget -from:nobody@x.test").missing,
            3,
            "non-mailbox filters ignored"
        );

        // Plan check for each scope shape: the scoped shapes must seek
        // idx_messages_uid by prefix; the all-accounts shape may scan only
        // an index (covering), never the messages table itself.
        let conn = db.lock_conn();
        for (by_acct, by_mb, args) in [
            (true, true, vec!["acc1", "acc1:INBOX"]),
            (true, false, vec!["acc1"]),
            (false, false, vec![]),
        ] {
            let sql = format!("EXPLAIN QUERY PLAN {}", body_scope_sql(by_acct, by_mb, ""));
            let plan: Vec<String> = conn
                .prepare(&sql)
                .unwrap()
                .query_map(params_from_iter(args), |r| r.get::<_, String>(3))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            let plan = plan.join("\n");
            assert!(
                plan.contains("INDEX"),
                "shape ({by_acct},{by_mb}) plan: {plan}"
            );
            assert!(
                !plan.contains("SCAN m\n") && !plan.ends_with("SCAN m"),
                "shape ({by_acct},{by_mb}) full-table scan: {plan}"
            );
        }
    }

    #[test]
    fn sized_missing_uid_query_skips_oversized_and_keeps_unknown_sizes() {
        let db = fresh_db();
        seed_account_mailbox(&db);

        // Newest-first: uid 3 oversized, uid 2 small, uid 1 size-unknown.
        let mut oversized = msg(3, "oversized", 3000);
        oversized.text_body = None;
        oversized.html_body = None;
        oversized.sanitized_html = None;
        oversized.size_bytes = Some(10_000_000);
        db.upsert_message(&oversized).unwrap();

        let mut small = msg(2, "small", 2000);
        small.text_body = None;
        small.html_body = None;
        small.sanitized_html = None;
        small.size_bytes = Some(2_048);
        db.upsert_message(&small).unwrap();

        let mut unknown = msg(1, "unknown size", 1000);
        unknown.text_body = None;
        unknown.html_body = None;
        unknown.sanitized_html = None;
        unknown.size_bytes = None;
        db.upsert_message(&unknown).unwrap();

        // The oversized newest row must not starve the smaller rows behind it.
        let rows = db
            .list_body_missing_uids_with_sizes("acc1", "acc1:INBOX", 25, 1_000_000)
            .unwrap();
        assert_eq!(
            rows,
            vec![(2, Some(2_048)), (1, None)],
            "oversized rows are excluded; unknown sizes stay eligible"
        );

        // A generous budget includes everything, newest first.
        let rows = db
            .list_body_missing_uids_with_sizes("acc1", "acc1:INBOX", 25, 100_000_000)
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].0, 3);

        // Zero/negative budgets and limits short-circuit.
        assert!(db
            .list_body_missing_uids_with_sizes("acc1", "acc1:INBOX", 25, 0)
            .unwrap()
            .is_empty());
        assert!(db
            .list_body_missing_uids_with_sizes("acc1", "acc1:INBOX", 0, 1_000)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn descriptor_retry_and_hydration_preserve_local_intent_and_rich_content() {
        let db = fresh_db();
        seed_account_mailbox(&db);

        let mut hydrated = msg(1, "hydrated", 1000);
        hydrated.attachments = Some(vec![]);
        hydrated.references = vec!["root@example.test".into()];
        hydrated.addresses.push(Address {
            kind: "reply-to".into(),
            name: None,
            address: "replies@local.test".into(),
        });
        db.upsert_message(&hydrated).unwrap();
        db.mark_read(&hydrated.id, true).unwrap();
        db.set_flagged(&hydrated.id, true).unwrap();

        let mut descriptor_retry = msg(1, "descriptor retry", 1001);
        descriptor_retry.text_body = None;
        descriptor_retry.html_body = None;
        descriptor_retry.sanitized_html = None;
        descriptor_retry.attachments = None;
        descriptor_retry.is_read = false;
        descriptor_retry.is_starred = false;
        descriptor_retry.addresses.truncate(1);
        descriptor_retry.references.clear();
        db.upsert_message(&descriptor_retry).unwrap();

        let after_descriptor = db.get_message_detail(&hydrated.id).unwrap().unwrap();
        assert!(after_descriptor.header.is_read);
        assert!(after_descriptor.header.is_starred);
        assert_eq!(after_descriptor.text_body.as_deref(), Some("hello world"));
        assert_eq!(after_descriptor.addresses.len(), 3);
        assert_eq!(db.search_messages("acc1", "hello", 10).unwrap().len(), 1);
        let reference_count = db
            .lock_conn()
            .query_row(
                "SELECT COUNT(*) FROM message_references WHERE message_id = ?1",
                params![hydrated.id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(reference_count, 1);

        // A later full-body hydration may refresh rich content, but fetched
        // remote flags must still not overwrite unreplayed local intent.
        let mut rehydrated = hydrated.clone();
        rehydrated.text_body = Some("replacement body".into());
        rehydrated.is_read = false;
        rehydrated.is_starred = false;
        db.upsert_message(&rehydrated).unwrap();
        let after_hydration = db.get_message_detail(&hydrated.id).unwrap().unwrap();
        assert!(after_hydration.header.is_read);
        assert!(after_hydration.header.is_starred);
        assert_eq!(
            after_hydration.text_body.as_deref(),
            Some("replacement body")
        );
    }

    #[test]
    fn authoritative_empty_or_attachment_only_body_is_marked_hydrated() {
        let db = fresh_db();
        seed_account_mailbox(&db);

        let mut empty = msg(1, "empty", 1000);
        empty.text_body = None;
        empty.html_body = None;
        empty.sanitized_html = None;
        empty.attachments = Some(vec![]);
        db.upsert_message(&empty).unwrap();

        let mut attachment_only = msg(2, "attachment only", 1001);
        attachment_only.text_body = None;
        attachment_only.html_body = None;
        attachment_only.sanitized_html = None;
        attachment_only.attachments = Some(vec![AttachmentDescriptor {
            ordinal: 0,
            filename: Some("payload.bin".into()),
            content_type: Some("application/octet-stream".into()),
            size_bytes: Some(4),
            section: "1".into(),
            content_id: None,
            is_inline: false,
            transfer_encoding: None,
        }]);
        db.upsert_message(&attachment_only).unwrap();

        assert!(
            db.get_message_detail(&empty.id)
                .unwrap()
                .unwrap()
                .header
                .has_body
        );
        assert!(
            db.get_message_detail(&attachment_only.id)
                .unwrap()
                .unwrap()
                .header
                .has_body
        );
        assert!(db
            .list_body_missing_uids("acc1", "acc1:INBOX", 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn upsert_message_persists_attachment_section_and_inline_metadata() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let msg = NewMessage {
            id: "acc1:m1".into(),
            account_id: "acc1".into(),
            mailbox_id: "acc1:INBOX".into(),
            provider_uid: Some(1),
            rfc822_message_id: None,
            subject: Some("s".into()),
            from_name: None,
            from_address: None,
            sent_at: None,
            snippet: None,
            size_bytes: None,
            has_attachments: true,
            attachments: Some(vec![AttachmentDescriptor {
                ordinal: 0,
                filename: Some("logo.png".into()),
                content_type: Some("image/png".into()),
                size_bytes: Some(5),
                section: "2.2".into(),
                content_id: Some("logo123".into()),
                is_inline: true,
                transfer_encoding: Some("base64".into()),
            }]),
            is_read: false,
            is_starred: false,
            addresses: vec![],
            text_body: None,
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: vec![],
        };
        db.upsert_message(&msg).unwrap();

        let attachments = db.list_message_attachments("acc1:m1").unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].section, "2.2");
        assert_eq!(attachments[0].content_id.as_deref(), Some("logo123"));
        assert!(attachments[0].is_inline);
        assert_eq!(attachments[0].transfer_encoding.as_deref(), Some("base64"));
    }

    #[test]
    fn upsert_paperclip_ignores_inline_cid_parts() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let desc = |inline: bool, cid: Option<&str>, name: &str| AttachmentDescriptor {
            ordinal: 0,
            filename: Some(name.into()),
            content_type: Some("application/octet-stream".into()),
            size_bytes: Some(5),
            section: "2".into(),
            content_id: cid.map(str::to_owned),
            is_inline: inline,
            transfer_encoding: None,
        };
        let mk = |id: &str, uid: i64, atts: Vec<AttachmentDescriptor>| NewMessage {
            id: id.into(),
            account_id: "acc1".into(),
            mailbox_id: "acc1:INBOX".into(),
            provider_uid: Some(uid),
            rfc822_message_id: None,
            subject: Some("s".into()),
            from_name: None,
            from_address: None,
            sent_at: None,
            snippet: None,
            size_bytes: None,
            // The envelope guess is wrong on purpose: the descriptors win.
            has_attachments: true,
            attachments: Some(atts),
            is_read: false,
            is_starred: false,
            addresses: vec![],
            text_body: None,
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: vec![],
        };
        db.upsert_message(&mk(
            "acc1:cid",
            11,
            vec![desc(true, Some("c1"), "logo.png")],
        ))
        .unwrap();
        db.upsert_message(&mk("acc1:pdf", 12, vec![desc(false, None, "a.pdf")]))
            .unwrap();
        let flag = |id: &str| -> i64 {
            db.lock_conn()
                .query_row(
                    "SELECT has_attachments FROM messages WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(flag("acc1:cid"), 0);
        assert_eq!(flag("acc1:pdf"), 1);
        // The inline descriptor is still stored.
        assert_eq!(db.list_message_attachments("acc1:cid").unwrap().len(), 1);
    }

    /// #3443: the detail carries the message's own References chain, oldest first, so a reply
    /// can send the whole chain instead of only the parent's Message-ID.
    fn sample_invite(uid: &str, sequence: i64, method: &str) -> CalendarInvite {
        CalendarInvite {
            uid: uid.into(),
            sequence,
            method: method.into(),
            summary: Some("Sync".into()),
            dtstart: None,
            dtend: None,
            organizer_email: Some("alice@local.test".into()),
            organizer_name: None,
            attendees: vec![],
            location: None,
            description: None,
            our_response: None,
            responded_at: None,
            recurrence_id: None,
            superseded: None,
            possibly_superseded: None,
        }
    }

    /// Mark `message_id` as authenticated (DMARC pass) or not.
    fn set_dmarc(db: &Database, message_id: &str, pass: bool) {
        let r = if pass {
            TrustResult::Pass
        } else {
            TrustResult::Fail
        };
        db.upsert_message_trust(
            message_id,
            &TrustSignals {
                spf: r,
                dkim: r,
                dmarc: r,
                authserv_id: None,
            },
        )
        .unwrap();
    }

    /// #3446: a later invite (higher SEQUENCE) or a CANCEL for the same meeting marks the earlier
    /// message as superseded; the latest one and unrelated meetings are not.
    #[test]
    fn calendar_invites_are_superseded_by_a_later_sequence_or_cancel() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        for (uid, id) in [(5, "first"), (6, "update"), (7, "cancel"), (8, "other")] {
            db.upsert_message(&msg(uid, id, 1000)).unwrap();
        }
        let m = |n: i64| format!("acc1:INBOX:{n}");
        db.upsert_calendar_invite(&m(5), &sample_invite("u1", 0, "REQUEST"))
            .unwrap();
        // Alone: not superseded.
        assert_eq!(
            db.get_calendar_invite(&m(5)).unwrap().unwrap().superseded,
            None
        );
        db.upsert_calendar_invite(&m(6), &sample_invite("u1", 1, "REQUEST"))
            .unwrap();
        db.upsert_calendar_invite(&m(8), &sample_invite("u2", 9, "REQUEST"))
            .unwrap();
        set_dmarc(&db, &m(6), true);
        set_dmarc(&db, &m(7), true);
        assert_eq!(
            db.get_calendar_invite(&m(5))
                .unwrap()
                .unwrap()
                .superseded
                .as_deref(),
            Some("updated")
        );
        assert_eq!(
            db.get_calendar_invite(&m(6)).unwrap().unwrap().superseded,
            None
        );
        assert_eq!(
            db.get_calendar_invite(&m(8)).unwrap().unwrap().superseded,
            None
        );
        // A CANCEL at the same SEQUENCE as the newest request cancels it.
        db.upsert_calendar_invite(&m(7), &sample_invite("u1", 1, "CANCEL"))
            .unwrap();
        assert_eq!(
            db.get_calendar_invite(&m(6))
                .unwrap()
                .unwrap()
                .superseded
                .as_deref(),
            Some("cancelled")
        );
        assert_eq!(
            db.get_calendar_invite(&m(5))
                .unwrap()
                .unwrap()
                .superseded
                .as_deref(),
            Some("cancelled")
        );
        assert_eq!(
            db.get_calendar_invite(&m(7)).unwrap().unwrap().superseded,
            None,
            "a CANCEL itself is never 'superseded'"
        );
    }

    /// Security: nobody who merely knows a UID can cancel or replace a meeting.
    #[test]
    fn only_an_authenticated_organizer_supersedes_everything_else_is_advisory() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let m = |n: i64| format!("acc1:INBOX:{n}");
        let from = |n: i64, who: &str| {
            let mut x = msg(n, "later", 1000);
            x.from_address = Some(who.into());
            db.upsert_message(&x).unwrap();
        };
        db.upsert_message(&msg(5, "real", 1000)).unwrap();
        db.upsert_calendar_invite(&m(5), &sample_invite("u1", 0, "REQUEST"))
            .unwrap();
        let state = |n: i64| {
            let i = db.get_calendar_invite(&m(n)).unwrap().unwrap();
            (i.superseded, i.possibly_superseded)
        };

        // Spoofed CANCEL: right organizer and From, but DMARC failed -> advisory only.
        from(6, "alice@local.test");
        db.upsert_calendar_invite(&m(6), &sample_invite("u1", 5, "CANCEL"))
            .unwrap();
        set_dmarc(&db, &m(6), false);
        assert_eq!(state(5), (None, Some("cancelled".into())));

        // Same domain, different user, DMARC pass: its organizer is that other user -> ignored.
        from(7, "mallory@local.test");
        let mut other = sample_invite("u1", 6, "CANCEL");
        other.organizer_email = Some("mallory@local.test".into());
        db.upsert_calendar_invite(&m(7), &other).unwrap();
        set_dmarc(&db, &m(7), true);
        assert_eq!(
            state(5),
            (None, Some("cancelled".into())),
            "only the DMARC-failed one is advisory"
        );

        // Right organizer but sent by a same-domain colleague, DMARC pass: not auto.
        from(8, "colleague@local.test");
        db.upsert_calendar_invite(&m(8), &sample_invite("u1", 7, "CANCEL"))
            .unwrap();
        set_dmarc(&db, &m(8), true);
        assert_eq!(state(5).0, None, "From must equal the organizer exactly");

        // A NULL organizer never supersedes, and an invite with none is never superseded.
        from(9, "alice@local.test");
        let mut none = sample_invite("u1", 8, "REQUEST");
        none.organizer_email = None;
        db.upsert_calendar_invite(&m(9), &none).unwrap();
        set_dmarc(&db, &m(9), true);
        assert_eq!(state(9), (None, None));
        assert_eq!(state(5).0, None);

        // The organizer, From == organizer, DMARC pass, higher SEQUENCE: supersedes.
        from(10, "Alice@Local.Test");
        db.upsert_calendar_invite(&m(10), &sample_invite("u1", 9, "REQUEST"))
            .unwrap();
        set_dmarc(&db, &m(10), true);
        assert_eq!(state(5), (Some("updated".into()), None));
    }

    /// A cancelled single occurrence must not mark the whole series cancelled, and RECURRENCE-ID
    /// round-trips through the table.
    #[test]
    fn recurrence_id_round_trips_and_scopes_supersession() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(5, "series", 1000)).unwrap();
        db.upsert_message(&msg(6, "one occurrence cancelled", 1000))
            .unwrap();
        let series = sample_invite("u1", 0, "REQUEST");
        let mut one = sample_invite("u1", 1, "CANCEL");
        one.recurrence_id = Some(CalendarInviteDateTime {
            raw: "20260930T090000".into(),
            tzid: Some("Europe/London".into()),
            is_utc: false,
            is_date_only: false,
        });
        db.upsert_calendar_invite("acc1:INBOX:5", &series).unwrap();
        db.upsert_calendar_invite("acc1:INBOX:6", &one).unwrap();
        let got = db.get_calendar_invite("acc1:INBOX:6").unwrap().unwrap();
        assert_eq!(got.recurrence_id, one.recurrence_id);
        assert_eq!(
            db.get_calendar_invite("acc1:INBOX:5")
                .unwrap()
                .unwrap()
                .superseded,
            None,
            "an occurrence-level cancel does not cancel the series"
        );
    }

    #[test]
    fn get_message_detail_exposes_the_references_chain() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let mut m = msg(5, "deep thread", 1000);
        m.references = vec!["<root@x>".into(), "<mid@x>".into()];
        db.upsert_message(&m).unwrap();
        let detail = db.get_message_detail("acc1:INBOX:5").unwrap().unwrap();
        assert_eq!(detail.references, vec!["<root@x>", "<mid@x>"]);
        let plain = msg(6, "no refs", 1000);
        db.upsert_message(&plain).unwrap();
        assert!(db
            .get_message_detail("acc1:INBOX:6")
            .unwrap()
            .unwrap()
            .references
            .is_empty());
    }

    #[test]
    fn get_message_detail_includes_attachments() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let msg = NewMessage {
            id: "acc1:m2".into(),
            account_id: "acc1".into(),
            mailbox_id: "acc1:INBOX".into(),
            provider_uid: Some(2),
            rfc822_message_id: None,
            subject: Some("s".into()),
            from_name: None,
            from_address: None,
            sent_at: None,
            snippet: None,
            size_bytes: None,
            has_attachments: true,
            attachments: Some(vec![AttachmentDescriptor {
                ordinal: 0,
                filename: Some("report.pdf".into()),
                content_type: Some("application/pdf".into()),
                size_bytes: Some(1234),
                section: "3".into(),
                content_id: None,
                is_inline: false,
                transfer_encoding: None,
            }]),
            is_read: false,
            is_starred: false,
            addresses: vec![],
            text_body: Some("body".into()),
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: vec![],
        };
        db.upsert_message(&msg).unwrap();

        let detail = db
            .get_message_detail("acc1:m2")
            .unwrap()
            .expect("row exists");
        assert_eq!(detail.attachments.len(), 1);
        assert_eq!(
            detail.attachments[0].filename.as_deref(),
            Some("report.pdf")
        );
        assert_eq!(detail.attachments[0].section, "3");
    }

    #[test]
    fn set_unsubscribe_targets_is_reflected_in_get_message_detail() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "newsletter", 1000)).unwrap();

        db.set_unsubscribe_targets(
            "acc1:INBOX:1",
            Some("mailto:unsub@example.com?subject=unsubscribe"),
            Some("https://example.com/unsub?id=42"),
            true,
        )
        .unwrap();

        let detail = db.get_message_detail("acc1:INBOX:1").unwrap().unwrap();
        assert_eq!(
            detail.list_unsubscribe_mailto.as_deref(),
            Some("mailto:unsub@example.com?subject=unsubscribe")
        );
        assert_eq!(
            detail.list_unsubscribe_url.as_deref(),
            Some("https://example.com/unsub?id=42")
        );
        assert!(detail.list_unsubscribe_one_click);
    }

    #[test]
    fn a_message_with_no_unsubscribe_targets_reads_back_as_none() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "plain", 1000)).unwrap();

        let detail = db.get_message_detail("acc1:INBOX:1").unwrap().unwrap();
        assert!(detail.list_unsubscribe_mailto.is_none());
        assert!(detail.list_unsubscribe_url.is_none());
        assert!(!detail.list_unsubscribe_one_click);
    }

    #[test]
    fn set_unsubscribe_targets_errors_on_unknown_message_id() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let err = db
            .set_unsubscribe_targets("acc1:INBOX:missing", None, None, false)
            .unwrap_err();
        assert!(err.to_string().contains("unknown message id"), "{err}");
    }

    #[test]
    fn attachment_cache_round_trips_and_lru_orders_by_last_access() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        for (id, uid) in [("acc1:m1", 1_i64), ("acc1:m2", 2_i64)] {
            let msg = NewMessage {
                id: id.into(),
                account_id: "acc1".into(),
                mailbox_id: "acc1:INBOX".into(),
                provider_uid: Some(uid),
                rfc822_message_id: None,
                subject: None,
                from_name: None,
                from_address: None,
                sent_at: None,
                snippet: None,
                size_bytes: None,
                has_attachments: false,
                attachments: None,
                is_read: false,
                is_starred: false,
                addresses: vec![],
                text_body: None,
                sanitized_html: None,
                html_body: None,
                has_remote_content: false,
                references: vec![],
            };
            db.upsert_message(&msg).unwrap();
        }

        assert_eq!(db.attachment_cache_entry("acc1:m1", 0).unwrap(), None);

        db.upsert_attachment_cache_entry("acc1:m1", 0, "hash-a", 100, 1_000)
            .unwrap();
        db.upsert_attachment_cache_entry("acc1:m2", 0, "hash-b", 200, 2_000)
            .unwrap();

        let entry = db
            .attachment_cache_entry("acc1:m1", 0)
            .unwrap()
            .expect("row exists");
        assert_eq!(entry.content_hash, "hash-a");
        assert_eq!(entry.size_bytes, 100);

        assert_eq!(db.total_attachment_cache_bytes().unwrap(), 300);

        let lru = db.attachment_cache_hashes_by_lru().unwrap();
        assert_eq!(lru.len(), 2);
        assert_eq!(lru[0].content_hash, "hash-a", "older access sorts first");
        assert_eq!(lru[1].content_hash, "hash-b");

        db.touch_attachment_cache_access("acc1:m1", 0, 5_000)
            .unwrap();
        let lru_after_touch = db.attachment_cache_hashes_by_lru().unwrap();
        assert_eq!(
            lru_after_touch[0].content_hash, "hash-b",
            "hash-a is now most recent"
        );

        db.delete_attachment_cache_hash("hash-b").unwrap();
        assert_eq!(db.attachment_cache_entry("acc1:m2", 0).unwrap(), None);
        assert_eq!(db.total_attachment_cache_bytes().unwrap(), 100);
    }

    #[test]
    fn attachment_cache_row_cascades_with_its_message() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let msg = NewMessage {
            id: "acc1:m3".into(),
            account_id: "acc1".into(),
            mailbox_id: "acc1:INBOX".into(),
            provider_uid: Some(1),
            rfc822_message_id: None,
            subject: None,
            from_name: None,
            from_address: None,
            sent_at: None,
            snippet: None,
            size_bytes: None,
            has_attachments: false,
            attachments: None,
            is_read: false,
            is_starred: false,
            addresses: vec![],
            text_body: None,
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: vec![],
        };
        db.upsert_message(&msg).unwrap();
        db.upsert_attachment_cache_entry("acc1:m3", 0, "hash-c", 50, 1_000)
            .unwrap();
        db.delete_messages_for_mailbox("acc1", "acc1:INBOX")
            .unwrap();
        assert_eq!(db.attachment_cache_entry("acc1:m3", 0).unwrap(), None);
    }

    #[test]
    fn hydrated_attachment_descriptors_replace_atomically_and_cascade() {
        let db = fresh_db();
        seed_account_mailbox(&db);

        let mut hydrated = msg(1, "attachments", 1000);
        hydrated.has_attachments = true;
        hydrated.attachments = Some(vec![
            AttachmentDescriptor {
                ordinal: 0,
                filename: Some("brief.pdf".into()),
                content_type: Some("application/pdf".into()),
                size_bytes: Some(42),
                section: "1".into(),
                content_id: None,
                is_inline: false,
                transfer_encoding: None,
            },
            AttachmentDescriptor {
                ordinal: 1,
                filename: None,
                content_type: Some("application/octet-stream".into()),
                size_bytes: Some(7),
                section: "2".into(),
                content_id: None,
                is_inline: false,
                transfer_encoding: None,
            },
        ]);
        db.upsert_message(&hydrated).unwrap();
        assert_eq!(db.list_message_attachments(&hydrated.id).unwrap().len(), 2);

        // Descriptor-only refreshes may not know whether attachments exist and
        // never have an authoritative list, so they preserve hydrated metadata.
        let mut descriptor_only = hydrated.clone();
        descriptor_only.text_body = None;
        descriptor_only.html_body = None;
        descriptor_only.sanitized_html = None;
        descriptor_only.attachments = None;
        descriptor_only.has_attachments = false;
        db.upsert_message(&descriptor_only).unwrap();
        assert_eq!(db.list_message_attachments(&hydrated.id).unwrap().len(), 2);
        assert!(
            db.get_message_detail(&hydrated.id)
                .unwrap()
                .unwrap()
                .header
                .has_attachments
        );

        // Invalid replacement metadata rolls the whole upsert back, including
        // the delete which precedes insertion inside the transaction.
        let mut invalid = hydrated.clone();
        invalid.subject = Some("must roll back".into());
        invalid.attachments = Some(vec![AttachmentDescriptor {
            ordinal: -1,
            filename: Some("invalid.bin".into()),
            content_type: None,
            size_bytes: Some(1),
            section: "1".into(),
            content_id: None,
            is_inline: false,
            transfer_encoding: None,
        }]);
        assert!(db.upsert_message(&invalid).is_err());
        assert_eq!(db.list_message_attachments(&hydrated.id).unwrap().len(), 2);
        assert_eq!(
            db.get_message_detail(&hydrated.id)
                .unwrap()
                .unwrap()
                .header
                .subject
                .as_deref(),
            Some("attachments")
        );

        hydrated.attachments = Some(vec![AttachmentDescriptor {
            ordinal: 0,
            filename: Some("final.pdf".into()),
            content_type: Some("application/pdf".into()),
            size_bytes: Some(84),
            section: "1".into(),
            content_id: None,
            is_inline: false,
            transfer_encoding: None,
        }]);
        db.upsert_message(&hydrated).unwrap();
        let rows = db.list_message_attachments(&hydrated.id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].filename.as_deref(), Some("final.pdf"));
        assert_eq!(rows[0].size_bytes, Some(84));

        db.delete_messages_for_mailbox("acc1", "acc1:INBOX")
            .unwrap();
        assert!(db
            .list_message_attachments(&hydrated.id)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn sync_cursor_round_trip() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        assert!(db.get_sync_cursor("acc1", "acc1:INBOX").unwrap().is_none());
        db.set_sync_cursor(
            "acc1",
            "acc1:INBOX",
            &SyncCursor {
                uid_validity: Some(42),
                uid_next: Some(101),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
        let c = db.get_sync_cursor("acc1", "acc1:INBOX").unwrap().unwrap();
        assert_eq!(c.uid_validity, Some(42));
        assert_eq!(c.uid_next, Some(101));
    }

    #[test]
    fn detail_missing_returns_none() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        assert!(db.get_message_detail("nope").unwrap().is_none());
    }

    #[test]
    fn search_finds_by_subject_and_body() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "Quarterly report", 1000))
            .unwrap();
        db.upsert_message(&msg(2, "Lunch plans", 2000)).unwrap();

        let hits = db.search_messages("acc1", "report", 50).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].subject.as_deref(), Some("Quarterly report"));

        // Body ("hello world" for both) is indexed too.
        assert_eq!(db.search_messages("acc1", "hello", 50).unwrap().len(), 2);
        // Account scoping.
        assert_eq!(db.search_messages("other", "report", 50).unwrap().len(), 0);
    }

    /// G10 (#3633): a partial word finds the message while typing, and an
    /// exact quoted phrase does not prefix-match.
    #[test]
    fn search_matches_a_partial_word_as_a_prefix() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "Quarterly report", 1000))
            .unwrap();
        db.upsert_message(&msg(2, "Lunch plans", 2000)).unwrap();
        let hits = db.search_messages("acc1", "quart", 50).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].subject.as_deref(), Some("Quarterly report"));
        assert_eq!(
            db.search_messages("acc1", "rep lun", 50).unwrap().len(),
            0,
            "AND of prefixes"
        );
        assert_eq!(
            db.search_messages("acc1", "\"quart\"", 50).unwrap().len(),
            0
        );
        // A negated word stays exact: "-quart" does not hide "Quarterly".
        assert_eq!(db.search_messages("acc1", "-quart", 50).unwrap().len(), 2);
        assert_eq!(
            db.search_messages("acc1", "-quarterly", 50).unwrap().len(),
            1
        );
    }

    /// #3376: negated subject/before/after keep mail whose column is NULL.
    #[test]
    fn negated_search_filters_keep_null_subject_and_null_sent_at() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let mut bare = msg(1, "placeholder", 1000);
        bare.subject = None;
        bare.sent_at = None;
        db.upsert_message(&bare).unwrap();
        db.upsert_message(&msg(2, "Invoice March", 1_800_000_000))
            .unwrap();
        let ids = |q: &str| -> Vec<String> {
            let mut v: Vec<String> = db
                .search_messages("acc1", q, 50)
                .unwrap()
                .into_iter()
                .map(|h| h.id)
                .collect();
            v.sort();
            v
        };
        let bare_id = bare.id.clone();
        assert!(ids("-subject:invoice").contains(&bare_id));
        assert!(ids("-before:2000-01-01").contains(&bare_id));
        assert!(ids("-after:2100-01-01").contains(&bare_id));
        // positive forms still never match NULL
        assert!(!ids("subject:invoice").contains(&bare_id));
        assert!(!ids("before:2100-01-01").contains(&bare_id));
        assert!(!ids("after:2000-01-01").contains(&bare_id));
    }

    /// FP17: a quoted span is an ordered adjacent phrase; qualifiers fold
    /// case and accents; a decomposed (NFD) query finds the composed word.
    #[test]
    fn search_phrase_nfd_and_folded_qualifiers() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let mut a = msg(1, "Reunió de calç", 1000);
        a.text_body = Some("the quick brown fox".into());
        a.addresses[0].name = Some("Núria Solé".into());
        a.addresses[0].address = "nuria@local.test".into();
        let mut b = msg(2, "Other", 2000);
        b.text_body = Some("brown quick fox".into());
        b.addresses[0].name = Some("Muñoz".into());
        b.addresses[0].address = "munoz@local.test".into();
        db.upsert_message(&a).unwrap();
        db.upsert_message(&b).unwrap();
        let n = |q: &str| db.search_messages("acc1", q, 50).unwrap().len();
        assert_eq!(n("\"quick brown fox\""), 1);
        assert_eq!(n("\"brown quick\""), 1);
        assert_eq!(n("\"fox brown\""), 0);
        assert_eq!(n("body:\"quick brown\""), 1);
        assert_eq!(n("-\"quick brown\" fox"), 1);
        // NFD "calc" with combining cedilla finds the composed "calç".
        assert_eq!(n("cal\u{63}\u{327}"), 1);
        assert_eq!(n("subject:reunio"), 1);
        assert_eq!(n("subject:REUNIÓ"), 1);
        assert_eq!(n("subject:reuni\u{6f}\u{301}"), 1);
        assert_eq!(n("from:munoz"), 1);
        assert_eq!(n("from:MUÑOZ"), 1);
        assert_eq!(n("from:\"Nuria Sole\""), 1);
        assert_eq!(n("from:\"NÚRIA SOLÉ\""), 1);
        assert_eq!(n("from:nobody"), 0);
    }

    #[test]
    fn search_special_chars_do_not_error() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "Budget", 1000)).unwrap();
        for q in ["\"", "AND OR", "budget)", "   ", "* :", "a\"b"] {
            db.search_messages("acc1", q, 10)
                .unwrap_or_else(|e| panic!("query {q:?} errored: {e}"));
        }
        assert_eq!(db.search_messages("acc1", "budget", 10).unwrap().len(), 1);
    }

    #[test]
    fn search_reindexes_on_reupsert() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "Alpha", 1000)).unwrap();
        assert_eq!(db.search_messages("acc1", "Alpha", 10).unwrap().len(), 1);
        db.upsert_message(&msg(1, "Omega", 1000)).unwrap();
        assert_eq!(db.search_messages("acc1", "Alpha", 10).unwrap().len(), 0);
        assert_eq!(db.search_messages("acc1", "Omega", 10).unwrap().len(), 1);
    }

    #[test]
    fn fts_keeps_exactly_one_row_per_message_across_insert_reindex_and_reinsert() {
        // Guards the first-sync fast path: a NEW message skips the
        // (full-scan) FTS delete. That is only sound if no path can leave a
        // stale FTS row behind for a message id that is later re-inserted.
        let db = fresh_db();
        seed_account_mailbox(&db);
        let fts_rows = |db: &Database| -> i64 {
            db.lock_conn()
                .query_row("SELECT COUNT(*) FROM message_fts", [], |r| r.get(0))
                .unwrap()
        };
        db.upsert_message(&msg(1, "Alpha", 1000)).unwrap();
        assert_eq!(fts_rows(&db), 1, "new message indexed once");

        // Authoritative re-index (hydrated body) of an existing message.
        let mut hydrated = msg(1, "Alpha", 1000);
        hydrated.attachments = Some(vec![]);
        hydrated.text_body = Some("hydrated body".into());
        db.upsert_message(&hydrated).unwrap();
        assert_eq!(fts_rows(&db), 1, "re-index replaces, never duplicates");
        assert_eq!(db.search_messages("acc1", "hydrated", 10).unwrap().len(), 1);

        // Delete (trigger purges FTS) then the same id arrives again as new.
        db.purge_message("acc1", "acc1:INBOX:1").unwrap();
        assert_eq!(fts_rows(&db), 0);
        db.upsert_message(&msg(1, "Omega", 1000)).unwrap();
        assert_eq!(fts_rows(&db), 1, "re-inserted message indexed once");
        assert_eq!(db.search_messages("acc1", "Alpha", 10).unwrap().len(), 0);
        assert_eq!(db.search_messages("acc1", "Omega", 10).unwrap().len(), 1);
    }

    #[test]
    fn fts_follows_flag_and_envelope_repair_updates_without_losing_the_body() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let fts_rows = |db: &Database| -> i64 {
            db.lock_conn()
                .query_row("SELECT COUNT(*) FROM message_fts", [], |r| r.get(0))
                .unwrap()
        };
        let mut hydrated = msg(1, "=?utf-8?q?Caf=C3=A9_menu?=", 1000);
        hydrated.attachments = Some(vec![]);
        hydrated.text_body = Some("zebra crossing".into());
        db.upsert_message(&hydrated).unwrap();
        assert_eq!(db.search_messages("acc1", "zebra", 10).unwrap().len(), 1);

        // A flag change rewrites the messages row, never the index.
        db.mark_read("acc1:INBOX:1", true).unwrap();
        assert_eq!(db.search_messages("acc1", "zebra", 10).unwrap().len(), 1);

        // The encoded-word repair rewrites subject/sender in place on the
        // same FTS row; the body column must survive it.
        let decode = |s: &str| s.contains("Caf=C3=A9").then(|| "Café menu".to_string());
        while !db
            .repair_encoded_envelope_text_batch(10, &decode)
            .unwrap()
            .done
        {}
        assert_eq!(fts_rows(&db), 1, "updated in place, never duplicated");
        assert_eq!(db.search_messages("acc1", "café", 10).unwrap().len(), 1);
        assert_eq!(db.search_messages("acc1", "zebra", 10).unwrap().len(), 1);
        assert!(db.search_messages("acc1", "utf", 10).unwrap().is_empty());
    }

    #[test]
    fn migration_0038_preserves_search_results_and_ranking() {
        //: a v37 profile's message_fts rows live under the OLD
        // schema (message_id UNINDEXED, rowid = insertion order, no mapping
        // table). Reconstruct that state by hand, seed varied content,
        // capture search order, revert to it, then reopen (runs migration
        // 0038, rebuilding message_fts under the new rowid mapping) — the
        // same queries must return the exact same message ids in the exact
        // same order. bm25 ranks by tokenized term frequency alone, never by
        // physical rowid, so this is a real equivalence check, not a tautology.
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-repo-test-fts-migration-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);

        let db = Database::open(&path).expect("open");
        seed_account_mailbox(&db);
        for (uid, subject, body) in [
            (1, "Quarterly report", "budget budget budget forecast"),
            (2, "Lunch plans", "budget forecast"),
            (3, "Weekly sync", "unrelated topic entirely"),
            (4, "Budget review", "budget"),
            // Text-sorts before uid 2, so migration 0038's `ORDER BY id`
            // assigns it a different rowid than its insertion order.
            (10, "Budget appendix", "appendix budget"),
        ] {
            let mut m = msg(uid, subject, 1000 + uid);
            m.text_body = Some(body.into());
            db.upsert_message(&m).unwrap();
        }

        let ids = |hits: Vec<MessageHeader>| hits.into_iter().map(|h| h.id).collect::<Vec<_>>();
        let before_budget = ids(db.search_messages("acc1", "budget", 50).unwrap());
        let before_report = ids(db.search_messages("acc1", "report", 50).unwrap());
        assert!(!before_budget.is_empty());
        assert!(!before_report.is_empty());
        drop(db);

        // Revert in place to what a v37 profile actually had on disk: same
        // content, old rowid scheme, mapping table gone. This is exactly the
        // rebuild migration 0038 itself performs, run here by hand.
        {
            let db = Database::open(&path).expect("reopen for revert");
            let conn = db.lock_conn();
            conn.execute_batch(
                "DROP TRIGGER messages_before_delete_fts;
                 DROP TABLE message_fts_rowid;
                 DROP TABLE message_fts;
                 CREATE VIRTUAL TABLE message_fts USING fts5(
                     message_id UNINDEXED, subject, sender, body, tokenize = 'unicode61');
                 CREATE TRIGGER messages_after_delete_fts AFTER DELETE ON messages BEGIN
                     DELETE FROM message_fts WHERE message_id = OLD.id;
                 END;
                 INSERT INTO message_fts (message_id, subject, sender, body)
                 SELECT m.id, COALESCE(m.subject, ''),
                        TRIM(COALESCE(m.from_name, '') || ' ' || COALESCE(m.from_address, '')),
                        COALESCE(b.text_body, '')
                 FROM messages m LEFT JOIN message_bodies b ON b.message_id = m.id;
                 DROP TABLE thread_dirty_messages;
                 ALTER TABLE thread_rebuild_state DROP COLUMN force_full;
                 INSERT OR REPLACE INTO thread_rebuild_state (account_id, dirty)
                     VALUES ('acc1', 1);
                 DROP TABLE message_decode_skips;
                 ALTER TABLE sync_cursors DROP COLUMN descriptor_checkpoint_uid;
                 ALTER TABLE smart_mailboxes DROP COLUMN group_json;
                 ALTER TABLE smart_mailboxes DROP COLUMN scope_json;
                 ALTER TABLE saved_filters DROP COLUMN sort_order;
                 ALTER TABLE saved_filters DROP COLUMN stop_processing;
                 ALTER TABLE saved_filters DROP COLUMN enabled;
                 ALTER TABLE account_identities DROP COLUMN reply_to;
                 ALTER TABLE account_identities DROP COLUMN bcc;
                 ALTER TABLE account_identities DROP COLUMN is_default;
                 DROP TABLE tag_definitions;
                 DROP TABLE message_keywords;
                 DROP TABLE thread_mutes;
                 ALTER TABLE threads DROP COLUMN is_muted;
                 DROP TABLE work_items;
                 CREATE TABLE work_items (
                     id TEXT PRIMARY KEY,
                     project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                     title TEXT NOT NULL,
                     notes TEXT NOT NULL DEFAULT '',
                     state TEXT NOT NULL DEFAULT 'needs_reply'
                         CHECK (state IN ('needs_reply','waiting','delegated','blocked','done','someday')),
                     position INTEGER NOT NULL DEFAULT 0,
                     created_at INTEGER NOT NULL,
                     updated_at INTEGER NOT NULL
                 );
                 CREATE INDEX idx_work_items_project ON work_items(project_id, state, position);
                 CREATE TRIGGER links_work_item_delete AFTER DELETE ON work_items BEGIN
                     DELETE FROM links WHERE (src_type = 'work_item' AND src_id = OLD.id)
                                          OR (dst_type = 'work_item' AND dst_id = OLD.id);
                 END;
                 DROP TABLE suggestions;
                 DROP TABLE saved_views;
                 DROP TABLE project_timeline;
                 DROP INDEX idx_messages_account_rfc822;
                 DROP TABLE work_item_provenance;
                 DROP TABLE notification_outbox;
                 DROP TABLE vip_senders;
                 ALTER TABLE calendar_invites DROP COLUMN location;
                 ALTER TABLE calendar_invites DROP COLUMN description;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_raw;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_tzid;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_is_utc;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_is_date_only;
                 DROP TABLE pending_refetch;
                 DROP TABLE pending_rules;
                 DROP TABLE message_header_capture;
                 DROP TABLE message_headers;
                 DROP TABLE header_names;
                 DELETE FROM _migrations WHERE id IN (38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57);",
            )
            .unwrap();
            // The revert must be real, or the reopen below proves nothing.
            let has = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
            assert_eq!(has("SELECT COUNT(*) FROM sqlite_master WHERE name IN ('message_fts_rowid','thread_dirty_messages','message_decode_skips')"), 0);
            assert_eq!(has("SELECT COUNT(*) FROM pragma_table_info('sync_cursors') WHERE name = 'descriptor_checkpoint_uid'"), 0);
            assert_eq!(has("SELECT COUNT(*) FROM pragma_table_info('thread_rebuild_state') WHERE name = 'force_full'"), 0);
            assert_eq!(has("SELECT COUNT(*) FROM _migrations WHERE id >= 38"), 0);
        }

        let db2 = Database::open(&path).expect("upgrade reopen");
        assert_eq!(
            db2.schema_version().unwrap(),
            crate::latest_schema_version()
        );

        let after_budget = ids(db2.search_messages("acc1", "budget", 50).unwrap());
        let after_report = ids(db2.search_messages("acc1", "report", 50).unwrap());
        assert_eq!(
            before_budget, after_budget,
            "bm25 ranking must survive the rowid-keyed FTS migration"
        );
        assert_eq!(before_report, after_report);

        // Structures created by migrations 0038-0040 exist and are usable.
        {
            let conn = db2.lock_conn();
            let n = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
            for id in [38, 39, 40] {
                assert_eq!(
                    n(&format!("SELECT COUNT(*) FROM _migrations WHERE id = {id}")),
                    1
                );
            }
            // 0038: one mapping row per message, and each mapped FTS row
            // carries the same message id and the original content.
            assert_eq!(n("SELECT COUNT(*) FROM message_fts_rowid"), 5);
            assert_eq!(n("SELECT COUNT(*) FROM message_fts"), 5);
            assert_eq!(
                n(
                    "SELECT COUNT(*) FROM message_fts f JOIN message_fts_rowid r \
                   ON r.fts_rowid = f.rowid AND r.message_id = f.message_id"
                ),
                5,
                "every FTS rowid maps to its own message id"
            );
            let body: String = conn
                .query_row(
                    "SELECT f.body FROM message_fts f JOIN message_fts_rowid r \
                     ON r.fts_rowid = f.rowid WHERE r.message_id = 'acc1:INBOX:1'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(body, "budget budget budget forecast");
            // 0039: dirty-message table exists; a pre-existing dirty=1 flag
            // was promoted to force_full.
            assert_eq!(n("SELECT COUNT(*) FROM thread_dirty_messages"), 0);
            assert_eq!(
                n("SELECT force_full FROM thread_rebuild_state WHERE account_id = 'acc1'"),
                1
            );
            // 0040: checkpoint column present (NULL for old cursors) and the
            // skip table is writable.
            assert_eq!(
                n("SELECT COUNT(*) FROM pragma_table_info('sync_cursors') \
                   WHERE name = 'descriptor_checkpoint_uid'"),
                1
            );
            conn.execute(
                "INSERT INTO message_decode_skips \
                 (account_id, mailbox_id, uid, uid_validity, reason, next_retry_at) \
                 VALUES ('acc1', 'acc1:INBOX', 9, 1, 'test', 0)",
                [],
            )
            .unwrap();
            assert_eq!(n("SELECT COUNT(*) FROM message_decode_skips"), 1);
            // The new BEFORE DELETE trigger purges the mapped FTS row.
            conn.execute("DELETE FROM messages WHERE id = 'acc1:INBOX:1'", [])
                .unwrap();
            assert_eq!(n("SELECT COUNT(*) FROM message_fts"), 4);
            assert_eq!(n("SELECT COUNT(*) FROM message_fts_rowid"), 4);
        }

        drop(db2);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
    }

    #[test]
    fn delete_mailbox_purges_fts_index() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "Findme", 1000)).unwrap();
        assert_eq!(db.search_messages("acc1", "Findme", 10).unwrap().len(), 1);

        db.delete_messages_for_mailbox("acc1", "acc1:INBOX")
            .unwrap();
        assert_eq!(db.search_messages("acc1", "Findme", 10).unwrap().len(), 0);

        // The AFTER DELETE trigger must leave no orphaned FTS rows.
        let conn = db.lock_conn();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM message_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "FTS index purged when messages are deleted");
    }

    #[test]
    fn rebuild_threads_groups_by_references() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&threaded_msg(1, "Topic", 1000, "root@x", &[]))
            .unwrap();
        db.upsert_message(&threaded_msg(2, "Re: Topic", 2000, "r1@x", &["root@x"]))
            .unwrap();
        db.upsert_message(&threaded_msg(
            3,
            "Re: Topic",
            3000,
            "r2@x",
            &["r1@x", "root@x"],
        ))
        .unwrap();
        db.upsert_message(&threaded_msg(4, "Unrelated", 4000, "alone@x", &[]))
            .unwrap();

        db.rebuild_threads("acc1").unwrap();
        let threads = db.list_thread_summaries("acc1", 50, 0).unwrap();
        assert_eq!(threads.len(), 2, "one 3-message thread + one singleton");

        let big = threads
            .iter()
            .find(|t| t.message_count == 3)
            .expect("3-msg thread");
        assert_eq!(big.unread_count, 3);
        let msgs = db.get_thread_messages(&big.id).unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].id, "acc1:INBOX:1", "oldest (root) first");
        assert_eq!(msgs[2].id, "acc1:INBOX:3");

        // Rebuild is idempotent.
        db.rebuild_threads("acc1").unwrap();
        assert_eq!(db.list_thread_summaries("acc1", 50, 0).unwrap().len(), 2);
    }

    // -- S42: read path — headers carry tags/mute; tag: search ----------

    #[test]
    fn header_keywords_are_visible_only_hidden_ones_filtered() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "One", 1000)).unwrap();
        db.upsert_message(&msg(2, "Two", 2000)).unwrap();
        db.upsert_message(&msg(3, "Three", 3000)).unwrap();
        db.set_message_keyword("acc1:INBOX:1", "Work", true)
            .unwrap();
        db.set_message_keyword("acc1:INBOX:1", "$Junk", true)
            .unwrap();

        let headers = db
            .list_message_headers("acc1", "acc1:INBOX", 50, 0)
            .unwrap();
        let h1 = headers
            .iter()
            .find(|h| h.id == "acc1:INBOX:1")
            .expect("msg1");
        assert_eq!(h1.keywords, vec!["Work".to_string()], "$Junk is hidden");
        let h2 = headers
            .iter()
            .find(|h| h.id == "acc1:INBOX:2")
            .expect("msg2");
        assert!(h2.keywords.is_empty());
    }

    #[test]
    fn header_thread_muted_reflects_the_thread() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "One", 1000)).unwrap();
        db.rebuild_threads("acc1").unwrap();
        db.mute_thread("acc1:INBOX:1", true).unwrap();

        let headers = db
            .list_message_headers("acc1", "acc1:INBOX", 50, 0)
            .unwrap();
        let h1 = headers.iter().find(|h| h.id == "acc1:INBOX:1").unwrap();
        assert!(h1.thread_muted);
    }

    /// DR1: in a Drafts (or Sent) mailbox every header path carries the
    /// recipients as `to_display` (name when known, else address; an
    /// unaddressed draft gets `Some("")`), while Inbox headers stay `None`.
    #[test]
    fn drafts_headers_carry_the_recipients_on_every_list_path() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Drafts".into(),
            account_id: "acc1".into(),
            path: "Drafts".into(),
            name: "Drafts".into(),
            role: Some("drafts".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.upsert_message(&msg(1, "Inbox mail", 1000)).unwrap();
        let mut draft = msg(2, "Quarterly plan", 2000);
        draft.id = "acc1:Drafts:2".into();
        draft.mailbox_id = "acc1:Drafts".into();
        draft.addresses = vec![
            Address {
                kind: "from".into(),
                name: None,
                address: "test@local.test".into(),
            },
            Address {
                kind: "to".into(),
                name: Some("Bob Builder".into()),
                address: "bob@example.com".into(),
            },
            Address {
                kind: "to".into(),
                name: None,
                address: "carol@example.com".into(),
            },
            Address {
                kind: "cc".into(),
                name: None,
                address: "dave@example.com".into(),
            },
        ];
        db.upsert_message(&draft).unwrap();
        let mut empty = msg(3, "Unaddressed", 3000);
        empty.id = "acc1:Drafts:3".into();
        empty.mailbox_id = "acc1:Drafts".into();
        empty.addresses = vec![];
        db.upsert_message(&empty).unwrap();

        let want = Some("Bob Builder, carol@example.com".to_string());
        let by_offset = db
            .list_message_headers("acc1", "acc1:Drafts", 50, 0)
            .unwrap();
        let by_page = db
            .list_message_page("acc1", "acc1:Drafts", None, 50)
            .unwrap()
            .rows;
        let by_role = db.list_messages_by_role("drafts", 50, 0).unwrap();
        let by_role_page = db.list_role_page("drafts", None, 50).unwrap().rows;
        for rows in [by_offset, by_page, by_role, by_role_page] {
            assert_eq!(rows.len(), 2);
            let plan = rows.iter().find(|h| h.id == "acc1:Drafts:2").unwrap();
            assert_eq!(plan.subject.as_deref(), Some("Quarterly plan"));
            assert_eq!(plan.to_display, want);
            let blank = rows.iter().find(|h| h.id == "acc1:Drafts:3").unwrap();
            assert_eq!(blank.to_display, Some(String::new()));
        }
        let inbox = db
            .list_message_headers("acc1", "acc1:INBOX", 50, 0)
            .unwrap();
        assert_eq!(inbox[0].to_display, None);
    }

    #[test]
    fn header_from_vip_is_case_insensitive_on_every_header_path() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "One", 1000)).unwrap();
        db.upsert_message(&msg(2, "Two", 2000)).unwrap();
        // msg() sends from alice@local.test; store the VIP key in odd case.
        db.set_vip(" ALICE@Local.Test ", None, true, 0).unwrap();

        let headers = db
            .list_message_headers("acc1", "acc1:INBOX", 50, 0)
            .unwrap();
        assert_eq!(headers.len(), 2);
        assert!(headers.iter().all(|h| h.from_vip));
        let detail = db.get_message_detail("acc1:INBOX:1").unwrap().unwrap();
        assert!(detail.header.from_vip);
        assert!(detail.rfc822_message_id.is_some());
        let page = db
            .list_message_page("acc1", "acc1:INBOX", None, 50)
            .unwrap();
        assert!(page.rows.iter().all(|h| h.from_vip));

        db.set_vip("alice@local.test", None, false, 0).unwrap();
        let headers = db
            .list_message_headers("acc1", "acc1:INBOX", 50, 0)
            .unwrap();
        assert!(headers.iter().all(|h| !h.from_vip));
    }

    #[test]
    fn search_is_vip_finds_only_vip_senders() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "One", 1000)).unwrap();
        assert!(db.search_messages("acc1", "is:vip", 50).unwrap().is_empty());
        db.set_vip("alice@local.test", None, true, 0).unwrap();
        assert_eq!(db.search_messages("acc1", "is:vip", 50).unwrap().len(), 1);
        assert_eq!(
            db.search_messages("acc1", "is:vip is:unread", 50)
                .unwrap()
                .len(),
            1
        );
        assert!(db
            .search_messages("acc1", "-is:vip", 50)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn search_tag_finds_a_message_by_keyword_and_by_display_name() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "Invoice", 1000)).unwrap();
        db.upsert_message(&msg(2, "Other", 2000)).unwrap();
        db.set_message_keyword("acc1:INBOX:1", "Work", true)
            .unwrap();
        db.upsert_tag_definition(&castellum_models::TagDefinition {
            keyword: String::new(),
            keyword_key: String::new(),
            display_name: "Work stuff".into(),
            colour: "blue".into(),
            sort_order: 0,
        })
        .unwrap();
        db.set_message_keyword("acc1:INBOX:2", "Work_stuff", true)
            .unwrap();

        let by_keyword = db.search_messages_across_accounts("tag:work", 50).unwrap();
        assert_eq!(
            by_keyword.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
            vec!["acc1:INBOX:1"]
        );
        let by_display_name = db
            .search_messages_across_accounts("tag:\"Work stuff\"", 50)
            .unwrap();
        assert_eq!(
            by_display_name
                .iter()
                .map(|h| h.id.as_str())
                .collect::<Vec<_>>(),
            vec!["acc1:INBOX:2"]
        );
    }

    #[test]
    fn smart_mailbox_tag_query_counts_correctly() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "One", 1000)).unwrap();
        db.upsert_message(&msg(2, "Two", 2000)).unwrap();
        db.set_message_keyword("acc1:INBOX:1", "Work", true)
            .unwrap();
        let created = db
            .upsert_smart_mailbox(crate::SmartMailboxUpsert {
                id: None,
                name: "Tagged Work",
                query: "tag:Work",
                symbol: None,
                group: None,
                scope: None,
            })
            .unwrap();
        let recomputed = db.recompute_smart_mailbox_counts(&created.id).unwrap();
        assert_eq!(recomputed.cached_total, 1);
    }

    fn thread_is_muted(db: &Database, thread_id: &str) -> bool {
        let conn = db.lock_conn();
        conn.query_row(
            "SELECT is_muted FROM threads WHERE id = ?1",
            params![thread_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn mute_survives_rebuild_threads() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&threaded_msg(1, "Topic", 1000, "root@x", &[]))
            .unwrap();
        db.rebuild_threads("acc1").unwrap();
        db.mute_thread("acc1:INBOX:1", true).unwrap();
        assert!(thread_is_muted(&db, "t:acc1:INBOX:1"));

        db.rebuild_threads("acc1").unwrap();
        assert!(
            thread_is_muted(&db, "t:acc1:INBOX:1"),
            "mute must survive a rebuild"
        );
        db.rebuild_threads("acc1").unwrap();
        assert!(
            thread_is_muted(&db, "t:acc1:INBOX:1"),
            "and a second rebuild"
        );
    }

    #[test]
    fn reply_joins_muted_thread() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&threaded_msg(1, "Topic", 1000, "root@x", &[]))
            .unwrap();
        db.rebuild_threads("acc1").unwrap();
        db.mute_thread("acc1:INBOX:1", true).unwrap();

        // A reply referencing the muted root arrives; rebuild folds it in.
        db.upsert_message(&threaded_msg(2, "Re: Topic", 2000, "r1@x", &["root@x"]))
            .unwrap();
        db.rebuild_threads("acc1").unwrap();

        let threads = db.list_thread_summaries("acc1", 50, 0).unwrap();
        assert_eq!(threads.len(), 1, "root + reply share one thread");
        assert_eq!(threads[0].message_count, 2);
        assert!(
            thread_is_muted(&db, &threads[0].id),
            "the reply's arrival must not un-mute the thread it joined"
        );
    }

    #[test]
    fn unmute_clears_all_anchors() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&threaded_msg(1, "Topic", 1000, "root@x", &[]))
            .unwrap();
        db.upsert_message(&threaded_msg(2, "Re: Topic", 2000, "r1@x", &["root@x"]))
            .unwrap();
        db.rebuild_threads("acc1").unwrap();
        db.mute_thread("acc1:INBOX:1", true).unwrap();
        {
            let conn = db.lock_conn();
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM thread_mutes WHERE account_id = 'acc1'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 2, "both members' anchors are muted");
        }

        db.mute_thread("acc1:INBOX:1", false).unwrap();
        let conn = db.lock_conn();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM thread_mutes WHERE account_id = 'acc1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "unmute must clear every anchor, not just the root's");
        drop(conn);
        let threads = db.list_thread_summaries("acc1", 50, 0).unwrap();
        assert!(!thread_is_muted(&db, &threads[0].id));
    }

    /// Merge of S42 (PR #173) over #181/#183: thread rows are now written in
    /// place by the shared `write_thread_group`, on the incremental path as
    /// well as the full one. When an earlier message arrives and becomes the
    /// root, the conversation gets a NEW `t:<root>` id; that row must come
    /// out muted (recomputed from `thread_mutes`, not defaulted to 0), or a
    /// muted conversation silently un-mutes and its new arrivals stop being
    /// marked read by sync's `muted_among` pass.
    #[test]
    fn mute_follows_a_root_changing_merge_on_the_incremental_path() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        // The muted conversation: a reply whose parent has not arrived yet.
        db.upsert_message(&threaded_msg(2, "Re: Topic", 2000, "r1@x", &["root@x"]))
            .unwrap();
        // Unrelated mail, so one arrival stays under the incremental path's
        // dirty-fraction limit.
        for uid in 10..20 {
            db.upsert_message(&threaded_msg(
                uid,
                "Other",
                5000 + uid,
                &format!("o{uid}@x"),
                &[],
            ))
            .unwrap();
        }
        db.rebuild_threads("acc1").unwrap();
        db.mute_thread("acc1:INBOX:2", true).unwrap();
        assert!(thread_is_muted(&db, "t:acc1:INBOX:2"));

        // The earlier root arrives: the conversation's root, and so its
        // thread id, changes.
        db.upsert_message(&threaded_msg(1, "Topic", 1000, "root@x", &[]))
            .unwrap();
        let path = db.rebuild_threads_incremental("acc1").unwrap();
        assert!(
            matches!(path, ThreadRebuildPath::Incremental { .. }),
            "the incremental path must be the one under test: {path:?}"
        );
        let threads = db.list_thread_summaries("acc1", 50, 0).unwrap();
        let merged = threads
            .iter()
            .find(|t| t.message_count == 2)
            .expect("root + reply share one thread");
        assert_eq!(merged.id, "t:acc1:INBOX:1", "the root changed");
        assert!(
            thread_is_muted(&db, &merged.id),
            "a root-changing merge must keep the conversation muted"
        );
        assert_eq!(
            db.muted_among(&["acc1:INBOX:1".to_string(), "acc1:INBOX:10".to_string()])
                .unwrap(),
            vec!["acc1:INBOX:1".to_string()],
            "the new arrival reads as muted; unrelated mail does not"
        );
        assert_eq!(
            actual_projection(&db, "acc1"),
            expected_projection(&db, "acc1"),
            "incremental result (is_muted included) matches a full rebuild"
        );
    }

    #[test]
    fn rebuild_threads_with_muted_anchors_stays_within_budget() {
        // Guards the S42 mute lookup against an O(n * muted) blowup: loading
        // `thread_mutes` once into a `HashSet` per rebuild must cost roughly
        // the same whether 0 or 50 anchors are muted.
        let n = 4000;
        let db_plain = fresh_db();
        seed_account_mailbox(&db_plain);
        for uid in 1..=n {
            db_plain
                .upsert_message(&msg(uid, "Perf", 1_000_000 + uid))
                .unwrap();
        }
        let start = std::time::Instant::now();
        db_plain.rebuild_threads("acc1").unwrap();
        let plain_elapsed = start.elapsed();

        let db_muted = fresh_db();
        seed_account_mailbox(&db_muted);
        for uid in 1..=n {
            db_muted
                .upsert_message(&msg(uid, "Perf", 1_000_000 + uid))
                .unwrap();
        }
        db_muted.rebuild_threads("acc1").unwrap();
        for uid in 1..=50 {
            db_muted
                .mute_thread(&format!("acc1:INBOX:{uid}"), true)
                .unwrap();
        }
        let start = std::time::Instant::now();
        db_muted.rebuild_threads("acc1").unwrap();
        let muted_elapsed = start.elapsed();

        let ratio = muted_elapsed.as_secs_f64() / plain_elapsed.as_secs_f64().max(0.000_001);
        assert!(
            ratio <= 1.5,
            "rebuild_threads with 50 muted anchors took {muted_elapsed:?} vs {plain_elapsed:?} \
             unmuted (ratio {ratio:.2}) — expected <= 1.5x"
        );
    }

    /// S64 review fix (migration 0037): `mark_read`/`mark_all_read`/
    /// `enqueue_move` (archive/delete/move)/`empty_trash` set the per-account
    /// "threads dirty" marker in the SAME transaction as the mutation, and
    /// `rebuild_threads` clears it in the same transaction as the rebuild —
    /// so a caller (sync.rs) can skip a rebuild only when nothing has moved
    /// a thread aggregate since the last one. extended this to
    /// EVERY new insert too (a brand-new message always needs a thread
    /// assignment, which is now the incremental engine's job, not a
    /// blanket post-sync rebuild) -- see the assertion right after the two
    /// `upsert_message` calls below, which flipped from "not dirty" for
    /// exactly that reason.
    #[test]
    fn local_mutations_set_threads_dirty_and_rebuild_clears_it() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "Hi", 1000)).unwrap();
        db.upsert_message(&msg(2, "Ho", 2000)).unwrap();

        // A brand-new message always needs a thread assignment, so the two
        // inserts above already marked the account dirty.
        assert!(db.threads_dirty("acc1").unwrap());
        db.rebuild_threads("acc1").unwrap();
        assert!(
            !db.threads_dirty("acc1").unwrap(),
            "rebuild_threads must not itself mark dirty"
        );

        // mark_read moves unread_count.
        db.mark_read("acc1:INBOX:1", true).unwrap();
        assert!(db.threads_dirty("acc1").unwrap());
        db.rebuild_threads("acc1").unwrap();
        assert!(
            !db.threads_dirty("acc1").unwrap(),
            "rebuild clears the marker"
        );

        // mark_all_read moves unread_count too.
        db.mark_all_read("acc1", "acc1:INBOX").unwrap();
        assert!(db.threads_dirty("acc1").unwrap());
        db.rebuild_threads("acc1").unwrap();
        assert!(!db.threads_dirty("acc1").unwrap());

        // A no-op mark_all_read (nothing left unread) must NOT mark dirty —
        // nothing changed for `rebuild_threads` to fix.
        db.mark_all_read("acc1", "acc1:INBOX").unwrap();
        assert!(
            !db.threads_dirty("acc1").unwrap(),
            "a mark_all_read with nothing to change is not a mutation"
        );

        // delete_message (move to Trash) removes a thread member. Message
        // 2 is the sole member of its own thread (no shared rfc822 id, no
        // References to/from message 1), so the departure (;
        // `prepare_message_departures` + `sweep_emptied_threads`) resolves
        // it FULLY inline -- sweeping the now-empty `threads` row in the
        // same transaction -- with no pending recompute left for a rebuild
        // to fix, hence dirty stays false.
        let thread_count_before = db.list_thread_summaries("acc1", 50, 0).unwrap().len();
        db.delete_message("acc1:INBOX:2").unwrap();
        assert!(
            !db.threads_dirty("acc1").unwrap(),
            "a singleton thread's departure is resolved inline, not deferred"
        );
        assert_eq!(
            db.list_thread_summaries("acc1", 50, 0).unwrap().len(),
            thread_count_before - 1,
            "the now-empty thread row must be gone immediately"
        );

        // Give message 3 a sibling in the same thread (shared rfc822 id
        // stands in for a References chain here), then delete the sibling:
        // now there IS a remaining member whose aggregates need
        // recomputing, so this time the account goes dirty.
        db.upsert_message(&msg(4, "Hey again", 3500)).unwrap();
        let mut sibling = msg(5, "Hey once more", 3600);
        sibling.rfc822_message_id = Some("<4@local.test>".into());
        db.upsert_message(&sibling).unwrap();
        db.rebuild_threads("acc1").unwrap();
        db.delete_message("acc1:INBOX:5").unwrap();
        assert!(
            db.threads_dirty("acc1").unwrap(),
            "message 4 is still there and its thread's aggregates are now stale"
        );
        db.rebuild_threads("acc1").unwrap();
        assert!(!db.threads_dirty("acc1").unwrap());

        // set_flagged (starred) touches no thread aggregate, so it must NOT
        // mark dirty — see its doc comment.
        db.upsert_message(&msg(3, "Hey", 3000)).unwrap();
        db.rebuild_threads("acc1").unwrap();
        db.set_flagged("acc1:INBOX:3", true).unwrap();
        assert!(
            !db.threads_dirty("acc1").unwrap(),
            "starring alone never makes the thread projection stale"
        );
    }

    /// splitmix64: a tiny, dependency-free, deterministic PRNG for the
    /// thread-rebuild property and stress tests below (no proptest/quickcheck
    /// dependency needed; a fixed seed reproduces a failure exactly).
    struct SplitMix(u64);
    impl SplitMix {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
            z ^ (z >> 31)
        }
        fn range(&mut self, n: usize) -> usize {
            (self.next_u64() % n as u64) as usize
        }
        fn bool(&mut self) -> bool {
            self.next_u64() & 1 == 0
        }
    }

    /// id, subject, first_message_at, last_message_at, message_count,
    /// unread_count, is_muted (S42) -- exactly the columns a thread rebuild
    /// writes.
    type ThreadRowSnapshot = (
        String,
        Option<String>,
        Option<i64>,
        Option<i64>,
        i64,
        i64,
        bool,
    );

    /// The whole thread projection for one account: every `threads` row and
    /// every message's `thread_id`, both sorted by id.
    #[derive(Debug, Clone, PartialEq)]
    struct Projection {
        threads: Vec<ThreadRowSnapshot>,
        membership: Vec<(String, Option<String>)>,
    }

    /// What the database holds right now.
    fn actual_projection(db: &Database, account_id: &str) -> Projection {
        let conn = db.lock_conn();
        let threads = conn
            .prepare(
                "SELECT id, subject, first_message_at, last_message_at,
                        message_count, unread_count, is_muted
                 FROM threads WHERE account_id = ?1 ORDER BY id",
            )
            .unwrap()
            .query_map(params![account_id], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let membership = conn
            .prepare("SELECT id, thread_id FROM messages WHERE account_id = ?1 ORDER BY id")
            .unwrap()
            .query_map(params![account_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        Projection {
            threads,
            membership,
        }
    }

    /// What a full `rebuild_threads` WOULD write from the current messages,
    /// references and flags -- computed read-only (the transaction is rolled
    /// back on drop), so a test can compare against it after every step
    /// without a full rebuild quietly repairing whatever the incremental
    /// path got wrong and hiding an accumulating divergence.
    fn expected_projection(db: &Database, account_id: &str) -> Projection {
        let mut conn = db.lock_conn();
        let tx = conn.transaction().unwrap();
        let msgs = load_account_thread_rows(&tx, account_id).unwrap();
        let refs = load_account_thread_refs(&tx, account_id).unwrap();
        let muted = load_account_muted_anchors(&tx, account_id).unwrap();
        let groups = compute_thread_groups(&msgs, &refs, &muted);
        let mut threads: Vec<ThreadRowSnapshot> = groups
            .iter()
            .map(|g| {
                (
                    g.thread_id.clone(),
                    g.subject.clone(),
                    g.first_message_at,
                    g.last_message_at,
                    g.message_count,
                    g.unread_count,
                    g.is_muted,
                )
            })
            .collect();
        threads.sort();
        let mut membership: Vec<(String, Option<String>)> = groups
            .iter()
            .flat_map(|g| {
                g.member_ids
                    .iter()
                    .map(|m| (m.clone(), Some(g.thread_id.clone())))
            })
            .collect();
        membership.sort();
        Projection {
            threads,
            membership,
        }
    }

    /// Messages whose `thread_id` names a `threads` row that does not exist
    /// -- `list_thread_summaries` silently drops them (PR #181 finding 4).
    fn dangling_thread_ids(db: &Database, account_id: &str) -> i64 {
        db.lock_conn()
            .query_row(
                "SELECT COUNT(*) FROM messages m
                 WHERE m.account_id = ?1 AND m.thread_id IS NOT NULL
                   AND NOT EXISTS (SELECT 1 FROM threads t WHERE t.id = m.thread_id)",
                params![account_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// `threads` rows no message points at any more -- a phantom
    /// conversation in the thread list (PR #181 finding 1).
    fn memberless_threads(db: &Database, account_id: &str) -> i64 {
        db.lock_conn()
            .query_row(
                "SELECT COUNT(*) FROM threads t
                 WHERE t.account_id = ?1
                   AND NOT EXISTS (SELECT 1 FROM messages m WHERE m.thread_id = t.id)",
                params![account_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn dirty_row_count(db: &Database, account_id: &str) -> i64 {
        db.lock_conn()
            .query_row(
                "SELECT COUNT(*) FROM thread_dirty_messages WHERE account_id = ?1",
                params![account_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// Run `f` with `hook` installed as this thread's [`REBUILD_GAP_HOOK`].
    fn with_rebuild_gap_hook<R>(
        hook: impl FnMut(&Database, &'static str) + 'static,
        f: impl FnOnce() -> R,
    ) -> R {
        REBUILD_GAP_HOOK.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
        let out = f();
        REBUILD_GAP_HOOK.with(|h| *h.borrow_mut() = None);
        out
    }

    fn seed_extra_mailbox(db: &Database, name: &str, role: Option<&str>) {
        db.upsert_mailbox(&Mailbox {
            id: format!("acc1:{name}"),
            account_id: "acc1".into(),
            path: name.into(),
            name: name.into(),
            role: role.map(str::to_string),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
    }

    /// Add `n` unrelated single-message conversations (uids 1000..), so a
    /// test's handful of dirty messages stays well under
    /// `INCREMENTAL_DIRTY_FRACTION_LIMIT` and the INCREMENTAL path -- not the
    /// small-account full-rebuild fallback -- is what the test exercises.
    fn pad_with_unrelated(db: &Database, n: i64) {
        for uid in 1_000..1_000 + n {
            db.upsert_message(&msg(uid, &format!("Unrelated {uid}"), 100_000 + uid))
                .unwrap();
        }
    }

    /// [`msg`] in another mailbox of `acc1`, with an rfc822 id unique to it.
    fn msg_in(mailbox: &str, uid: i64, subject: &str, sent_at: i64) -> NewMessage {
        let mut m = msg(uid, subject, sent_at);
        m.id = format!("acc1:{mailbox}:{uid}");
        m.mailbox_id = format!("acc1:{mailbox}");
        m.rfc822_message_id = Some(format!("<{mailbox}-{uid}@local.test>"));
        m
    }

    /// One live message in the property test's model of the mailbox: the
    /// conversation it belongs to, and the last version written for it (so
    /// a re-hydration or a sync copy into Trash can start from it).
    struct LiveMessage {
        conv: usize,
        m: NewMessage,
    }

    /// Property test (; strengthened after the PR #181 review,
    /// `~/.agents/skills/superpowers/test-driven-development`): after an
    /// arbitrary sequence of writes, `rebuild_threads_incremental` must leave
    /// EXACTLY the projection a full `rebuild_threads` would.
    ///
    /// The first version of this test passed against code with the Empty
    /// Trash orphan bug. Its random References joined most of the corpus
    /// into one giant component, and Trash held a fifth of the account, so
    /// every interesting round fell back to a full rebuild. This version:
    /// - Models real mail: many small conversations (replies cite the last
    ///   one to three ids of their own conversation, a few cite a second one
    ///   and MERGE them, some are duplicate deliveries), a 380-message
    ///   corpus that never drops below 300, and whole conversations
    ///   reaching Trash the way sync brings them there (the Inbox rows leave
    ///   via delete_message, then Trash copies with the same Message-IDs
    ///   arrive).
    /// - Asserts the INCREMENTAL path is what actually ran for the large
    ///   majority of rounds (the returned `ThreadRebuildPath`), so a
    ///   regression that quietly widens dirty sets into full rebuilds fails
    ///   here instead of passing on the fallback.
    /// - Uses a read-only oracle (`expected_projection`), so the incremental
    ///   state is never reset between rounds: a wrong aggregate or a phantom
    ///   row accumulates and is caught.
    /// - Covers every write path that can move a thread: inserts, merges and
    ///   splits via re-hydrated References, subject/sent_at changes, a
    ///   Message-ID discovered late, mark_read/mark_all_read/
    ///   reconcile_message_flags, move-to-Trash, purge_message (sync
    ///   vanish), empty_trash, delete_messages_for_mailbox and
    ///   clear_sync_cursor.
    ///
    /// After every round it also checks the reader-visible invariants (no
    /// dangling `thread_id`, no memberless thread row) and that a quiescent
    /// incremental pass leaves nothing dirty; every 25 rounds a full
    /// rebuild on the incrementally-maintained state must be a no-op.
    #[test]
    fn incremental_rebuild_matches_full_rebuild_after_random_mutations() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_extra_mailbox(&db, "Trash", Some("trash"));
        seed_extra_mailbox(&db, "Other", None);
        let account_id = "acc1";
        let mut rng = SplitMix(0xC0FF_EE5E_EDC0_FFEE_u64);
        let mut next_uid: i64 = 1;
        let mut live: Vec<LiveMessage> = Vec::new();
        // Per conversation: its home mailbox and every Message-ID posted to
        // it (ids of deleted messages stay: citing a message this client no
        // longer has is realistic).
        let mut convs: Vec<(&'static str, Vec<String>)> = Vec::new();
        // Messages whose Message-ID the envelope lacked, with the id a later
        // hydration will discover (replies already cite it).
        let mut late_ids: Vec<(String, String)> = Vec::new();

        // A References header citing the last one to three ids of `ids`.
        fn chain(rng: &mut SplitMix, ids: &[String]) -> Vec<String> {
            let take = (1 + rng.range(3)).min(ids.len());
            ids[ids.len() - take..].to_vec()
        }
        fn with_mailbox(m: &NewMessage, mailbox: &str, uid: i64) -> NewMessage {
            let mut copy = m.clone();
            copy.id = format!("acc1:{mailbox}:{uid}");
            copy.mailbox_id = format!("acc1:{mailbox}");
            copy.provider_uid = Some(uid);
            copy
        }
        let new_message = |rng: &mut SplitMix,
                           convs: &mut Vec<(&'static str, Vec<String>)>,
                           late_ids: &mut Vec<(String, String)>,
                           uid: i64|
         -> LiveMessage {
            let starts_conversation = convs.is_empty() || rng.range(10) < 3;
            let conv = if starts_conversation {
                let home = match rng.range(20) {
                    0 => "Trash",
                    1 => "Other",
                    _ => "INBOX",
                };
                convs.push((home, Vec::new()));
                convs.len() - 1
            } else {
                rng.range(convs.len())
            };
            let mut m = msg_in(
                convs[conv].0,
                uid,
                &format!("Subject {uid}"),
                1_000 + uid * 7,
            );
            if !starts_conversation {
                m.references = chain(rng, &convs[conv].1);
                if rng.range(15) == 0 {
                    let other = rng.range(convs.len());
                    m.references.extend(chain(rng, &convs[other].1));
                }
            }
            let own_id = m.rfc822_message_id.clone().unwrap();
            match rng.range(25) {
                0 if !convs[conv].1.is_empty() => {
                    // A duplicate delivery (Inbox + Sent copies).
                    let ids = &convs[conv].1;
                    m.rfc822_message_id = Some(ids[rng.range(ids.len())].clone());
                }
                1 => {
                    // No Message-ID in the envelope yet.
                    m.rfc822_message_id = None;
                    late_ids.push((m.id.clone(), own_id.clone()));
                    convs[conv].1.push(own_id);
                }
                _ => convs[conv].1.push(own_id),
            }
            LiveMessage { conv, m }
        };

        for _ in 0..380 {
            let uid = next_uid;
            next_uid += 1;
            let lm = new_message(&mut rng, &mut convs, &mut late_ids, uid);
            db.upsert_message(&lm.m).unwrap();
            live.push(lm);
        }
        db.rebuild_threads(account_id).unwrap();
        assert_eq!(
            actual_projection(&db, account_id),
            expected_projection(&db, account_id)
        );

        let mut incremental_rounds = 0usize;
        let mut full_rounds = 0usize;
        let mut min_live = live.len();
        for round in 0..400 {
            // Below 360 live messages a round always adds one, so no run of
            // deletions (Empty Trash, a mailbox purge) can shrink the
            // account until every dirty set is a large fraction of it.
            let action = if live.len() < 360 { 0 } else { rng.range(20) };
            match action {
                0..=4 => {
                    let uid = next_uid;
                    next_uid += 1;
                    let lm = new_message(&mut rng, &mut convs, &mut late_ids, uid);
                    db.upsert_message(&lm.m).unwrap();
                    live.push(lm);
                }
                5 => {
                    // Authoritative re-hydration rewriting References: none
                    // (a SPLIT when this message bridged two parts), its own
                    // conversation's chain, or another's (a MERGE).
                    let i = rng.range(live.len());
                    let target = match rng.range(3) {
                        0 => None,
                        1 => Some(live[i].conv),
                        _ => Some(rng.range(convs.len())),
                    };
                    let refs = target.map_or_else(Vec::new, |c| chain(&mut rng, &convs[c].1));
                    live[i].m.references = refs;
                    let mut m = live[i].m.clone();
                    m.attachments = Some(vec![]);
                    db.upsert_message(&m).unwrap();
                }
                6 => {
                    // Subject correction: a root's subject is the thread's.
                    let i = rng.range(live.len());
                    live[i].m.subject = Some(format!("Corrected {round}"));
                    db.upsert_message(&live[i].m).unwrap();
                }
                7 => {
                    // sent_at change (Date header replacing INTERNALDATE):
                    // can move the root, and with it the thread id.
                    let i = rng.range(live.len());
                    live[i].m.sent_at = Some(rng.range(4_000) as i64);
                    db.upsert_message(&live[i].m).unwrap();
                }
                8 => {
                    // A Message-ID discovered late: replies that already
                    // cite it join this message's thread.
                    let found = late_ids
                        .iter()
                        .position(|(id, _)| live.iter().any(|lm| lm.m.id == *id));
                    if let Some(pos) = found {
                        let (id, rfc) = late_ids.remove(pos);
                        let lm = live.iter_mut().find(|lm| lm.m.id == id).unwrap();
                        lm.m.rfc822_message_id = Some(rfc);
                        db.upsert_message(&lm.m).unwrap();
                    }
                }
                9 => {
                    let i = rng.range(live.len());
                    db.mark_read(&live[i].m.id, rng.bool()).unwrap();
                }
                10 => {
                    let i = rng.range(live.len());
                    db.reconcile_message_flags(account_id, &live[i].m.id, rng.bool(), rng.bool())
                        .unwrap();
                }
                11 => {
                    let i = rng.range(live.len());
                    let _ = db.set_flagged(&live[i].m.id, rng.bool());
                }
                12 => {
                    // Move one message to Trash (the local row leaves).
                    let lm = live.remove(rng.range(live.len()));
                    if lm.m.mailbox_id == "acc1:Trash" {
                        // Delete inside Trash is refused (S73: it is a
                        // confirmed permanent delete instead); the local row
                        // leaves the same way a confirmed expunge removes it.
                        assert!(db.delete_message(&lm.m.id).is_err());
                        db.finish_permanent_delete(&lm.m.id).unwrap();
                    } else {
                        db.delete_message(&lm.m.id).unwrap();
                    }
                }
                13 => {
                    // Sync reports one vanished.
                    let lm = live.remove(rng.range(live.len()));
                    db.purge_message(account_id, &lm.m.id).unwrap();
                }
                14 => {
                    // "Delete conversation": every member leaves its
                    // mailbox, then sync imports Trash copies carrying the
                    // same Message-IDs and References.
                    let conv = live[rng.range(live.len())].conv;
                    let (moving, staying): (Vec<LiveMessage>, Vec<LiveMessage>) = live
                        .drain(..)
                        .partition(|lm| lm.conv == conv && lm.m.mailbox_id != "acc1:Trash");
                    live = staying;
                    for lm in &moving {
                        db.delete_message(&lm.m.id).unwrap();
                    }
                    for lm in moving {
                        let uid = next_uid;
                        next_uid += 1;
                        let copy = with_mailbox(&lm.m, "Trash", uid);
                        db.upsert_message(&copy).unwrap();
                        live.push(LiveMessage { conv, m: copy });
                    }
                }
                15 => {
                    db.empty_trash(account_id).unwrap();
                    live.retain(|lm| lm.m.mailbox_id != "acc1:Trash");
                }
                16 => {
                    // mark_all_read on the small mailbox, Inbox (far past the
                    // dirty-fraction limit) now and then.
                    let mailbox = if rng.range(5) == 0 { "INBOX" } else { "Other" };
                    db.mark_all_read(account_id, &format!("acc1:{mailbox}"))
                        .unwrap();
                }
                17 if rng.range(10) == 0 => {
                    // A UIDVALIDITY reset purges a whole mailbox.
                    db.delete_messages_for_mailbox(account_id, "acc1:Other")
                        .unwrap();
                    live.retain(|lm| lm.m.mailbox_id != "acc1:Other");
                }
                18 if rng.range(10) == 0 => {
                    // The legacy empty_trash recovery purge.
                    db.clear_sync_cursor(account_id, "Trash").unwrap();
                    live.retain(|lm| lm.m.mailbox_id != "acc1:Trash");
                }
                _ => {
                    // Nothing new: the invariant must still hold.
                }
            }

            min_live = min_live.min(live.len());
            let path = db.rebuild_threads_incremental(account_id).unwrap();
            match path {
                ThreadRebuildPath::Incremental { .. } => incremental_rounds += 1,
                ThreadRebuildPath::Full(_) => full_rounds += 1,
                ThreadRebuildPath::Noop => {}
            }
            let ctx = format!("round {round} (action {action}, {path:?})");
            assert_eq!(
                actual_projection(&db, account_id),
                expected_projection(&db, account_id),
                "{ctx}: incremental projection diverged from a full rebuild"
            );
            assert_eq!(dangling_thread_ids(&db, account_id), 0, "{ctx}");
            assert_eq!(memberless_threads(&db, account_id), 0, "{ctx}");
            assert_eq!(
                dirty_row_count(&db, account_id),
                0,
                "{ctx}: a quiescent incremental pass must leave nothing dirty"
            );
            assert!(!db.threads_dirty(account_id).unwrap(), "{ctx}");
            if round % 25 == 24 {
                let before = actual_projection(&db, account_id);
                db.rebuild_threads(account_id).unwrap();
                assert_eq!(
                    actual_projection(&db, account_id),
                    before,
                    "{ctx}: a full rebuild on the incremental state must be a no-op"
                );
            }
        }

        assert!(
            min_live >= 300,
            "the corpus shrank to {min_live}: a round's dirty set is no longer a small fraction"
        );
        let ratio = incremental_rounds as f64 / (incremental_rounds + full_rounds) as f64;
        assert!(
            incremental_rounds >= 250 && ratio >= 0.9,
            "the incremental path must dominate: {incremental_rounds} incremental vs \
             {full_rounds} full-rebuild rounds ({ratio:.2})"
        );
    }

    /// Unit test for the chunk/fallback bounds themselves:
    /// a dirty set past `INCREMENTAL_DIRTY_FRACTION_LIMIT` of the account's
    /// messages takes the full-rebuild fallback rather than walking a
    /// closure that would no longer be cheaper than starting clean.
    #[test]
    fn incremental_rebuild_falls_back_to_full_past_the_dirty_fraction_limit() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        for uid in 1..=10 {
            db.upsert_message(&msg(uid, "Subject", 1000 + uid)).unwrap();
        }
        db.rebuild_threads("acc1").unwrap();
        assert!(!db.threads_dirty("acc1").unwrap());

        // 3 of 10 dirty (30%) is past the 20% limit -- every one of these
        // messages is its own singleton thread, so an exact-closure walk
        // would find nothing extra either; the fallback fires purely on
        // the fraction, not because the closure blew up.
        for uid in 1..=3 {
            db.mark_read(&format!("acc1:INBOX:{uid}"), true).unwrap();
        }
        assert_eq!(
            db.rebuild_threads_incremental("acc1").unwrap(),
            ThreadRebuildPath::Full(FullThreadRebuildReason::DirtyFraction)
        );
        assert!(
            !db.threads_dirty("acc1").unwrap(),
            "the fallback still fully resolves the account, same as any full rebuild"
        );
        for uid in 1..=10 {
            let unread: i64 = db
                .lock_conn()
                .query_row(
                    "SELECT unread_count FROM threads t
                     JOIN messages m ON m.thread_id = t.id WHERE m.id = ?1",
                    params![format!("acc1:INBOX:{uid}")],
                    |r| r.get(0),
                )
                .unwrap();
            let expect_unread = if uid <= 3 { 0 } else { 1 };
            assert_eq!(
                unread, expect_unread,
                "message {uid}'s thread aggregate is wrong"
            );
        }
    }

    /// Unit test: `force_full` (a mailbox purge / UIDVALIDITY reset) always
    /// takes the full-rebuild fallback, even with an otherwise-tiny dirty
    /// set, and both are cleared together.
    #[test]
    fn incremental_rebuild_honours_force_full() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "Hi", 1000)).unwrap();
        db.rebuild_threads("acc1").unwrap();

        db.delete_messages_for_mailbox("acc1", "acc1:INBOX")
            .unwrap();
        assert!(db.threads_dirty("acc1").unwrap());
        db.upsert_message(&msg(2, "Ho", 2000)).unwrap();
        assert_eq!(
            db.rebuild_threads_incremental("acc1").unwrap(),
            ThreadRebuildPath::Full(FullThreadRebuildReason::ForceFull)
        );
        assert!(!db.threads_dirty("acc1").unwrap());
        assert_eq!(db.list_thread_summaries("acc1", 50, 0).unwrap().len(), 1);
    }

    // --- PR #181 review findings (incremental thread rebuild) -------------
    //
    // One regression test per finding the Opus review reproduced, written
    // BEFORE the fix and watched failing against 57a7dc9d. The interleaving
    // ones use `REBUILD_GAP_HOOK` to put a write, a reader check or a second
    // rebuild at EVERY point a rebuild lets go of the connection mutex, so
    // they are deterministic rather than schedule-dependent.

    /// Finding 1 (CRITICAL): Empty Trash on a conversation whose every
    /// message is in Trash. Each departing message used to seed the OTHER as
    /// dirty (so neither saw an empty sibling list and deleted the thread
    /// row), and after the bulk DELETE those seeds no longer existed -- the
    /// closure found nothing and the phantom thread survived with its dirty
    /// markers cleared. The everyday "delete conversation, then Empty Trash".
    /// A second conversation that keeps a member outside Trash must still be
    /// recomputed (count, and root/id when the root was in Trash).
    #[test]
    fn empty_trash_of_a_whole_conversation_leaves_no_orphan_thread() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_extra_mailbox(&db, "Trash", Some("trash"));
        // Conversation 1: both messages in Trash.
        let a = msg_in("Trash", 1, "Plans", 1_000);
        let mut b = msg_in("Trash", 2, "Re: Plans", 2_000);
        b.references = vec![a.rfc822_message_id.clone().unwrap()];
        // Conversation 2: root in Trash, a reply still in the Inbox.
        let c = msg_in("Trash", 3, "Budget", 1_500);
        let mut d = msg(4, "Re: Budget", 2_500);
        d.references = vec![c.rfc822_message_id.clone().unwrap()];
        // An unrelated Inbox message, so the account keeps a projection and
        // the incremental path (not the first-run fallback) is what runs.
        let e = msg(5, "Unrelated", 500);
        for m in [&a, &b, &c, &d, &e] {
            db.upsert_message(m).unwrap();
        }
        pad_with_unrelated(&db, 40);
        db.rebuild_threads("acc1").unwrap();
        assert_eq!(actual_projection(&db, "acc1").threads.len(), 43);

        assert_eq!(db.empty_trash("acc1").unwrap(), 3);
        assert_eq!(
            memberless_threads(&db, "acc1"),
            0,
            "Empty Trash must not leave a thread row with no messages behind"
        );
        db.rebuild_threads_incremental("acc1").unwrap();
        assert_eq!(
            actual_projection(&db, "acc1"),
            expected_projection(&db, "acc1")
        );
        assert_eq!(actual_projection(&db, "acc1").threads.len(), 42);
        assert_eq!(memberless_threads(&db, "acc1"), 0);
        assert_eq!(dirty_row_count(&db, "acc1"), 0);
        assert!(!db.threads_dirty("acc1").unwrap());
    }

    /// Finding 1, bulk-purge half: a UIDVALIDITY reset
    /// (`delete_messages_for_mailbox`, and PR #180's
    /// `purge_mailbox_for_uid_space_reset`, which the sync engine actually
    /// calls on a reset) or the legacy empty-trash recovery
    /// (`clear_sync_cursor`) must not leave phantom thread rows readable
    /// until the next rebuild either -- they sweep the threads they emptied
    /// in the same transaction AND force the full-rebuild fallback. A
    /// folder-selection edit is
    /// the same kind of bulk purge, for both a removed plain folder and an
    /// unticked role folder whose row is kept.
    #[test]
    fn bulk_mailbox_purges_sweep_emptied_threads_and_force_a_full_rebuild() {
        for purge in [
            "delete_messages_for_mailbox",
            "clear_sync_cursor",
            "purge_mailbox_for_uid_space_reset",
            "apply_mailbox_selection",
            "apply_mailbox_selection (role)",
        ] {
            let db = fresh_db();
            seed_account_mailbox(&db);
            let role = (purge == "apply_mailbox_selection (role)").then_some("archive");
            seed_extra_mailbox(&db, "Other", role);
            let a = msg_in("Other", 1, "Only here", 1_000);
            let mut b = msg_in("Other", 2, "Re: only here", 2_000);
            b.references = vec![a.rfc822_message_id.clone().unwrap()];
            let c = msg_in("Other", 3, "Shared root", 1_200);
            let mut d = msg(4, "Re: shared root", 2_200);
            d.references = vec![c.rfc822_message_id.clone().unwrap()];
            for m in [&a, &b, &c, &d] {
                db.upsert_message(m).unwrap();
            }
            db.rebuild_threads("acc1").unwrap();

            match purge {
                "delete_messages_for_mailbox" => db
                    .delete_messages_for_mailbox("acc1", "acc1:Other")
                    .unwrap(),
                "purge_mailbox_for_uid_space_reset" => {
                    db.purge_mailbox_for_uid_space_reset("acc1", "acc1:Other", "Other", 2)
                        .unwrap();
                }
                "apply_mailbox_selection" | "apply_mailbox_selection (role)" => {
                    let (removed, _) = db
                        .apply_mailbox_selection("acc1", &["INBOX".to_string()], false)
                        .unwrap();
                    assert_eq!(removed, 1, "{purge}");
                }
                _ => db.clear_sync_cursor("acc1", "Other").unwrap(),
            }
            assert_eq!(
                memberless_threads(&db, "acc1"),
                0,
                "{purge}: an emptied conversation must not stay listed"
            );
            assert!(db.threads_dirty("acc1").unwrap(), "{purge}");
            db.rebuild_threads_incremental("acc1").unwrap();
            assert_eq!(
                actual_projection(&db, "acc1"),
                expected_projection(&db, "acc1"),
                "{purge}"
            );
            assert!(!db.threads_dirty("acc1").unwrap(), "{purge}");
        }
    }

    /// Two conversations plus a reply that merges them -- membership
    /// changes AND one thread id disappears, the shape on which two
    /// interleaved rebuilds collided on `threads.id`.
    fn seed_merge_scenario(db: &Database) {
        seed_account_mailbox(db);
        pad_with_unrelated(db, 40);
        let a = msg(1, "A", 1_000);
        let mut a2 = msg(2, "Re: A", 1_100);
        a2.references = vec![a.rfc822_message_id.clone().unwrap()];
        let b = msg(3, "B", 2_000);
        let unrelated = msg(4, "Unrelated", 3_000);
        for m in [&a, &a2, &b, &unrelated] {
            db.upsert_message(m).unwrap();
        }
        db.rebuild_threads("acc1").unwrap();
        let mut merge = msg(5, "Re: A and B", 4_000);
        merge.references = vec![
            a.rfc822_message_id.clone().unwrap(),
            b.rfc822_message_id.clone().unwrap(),
        ];
        db.upsert_message(&merge).unwrap();
        db.mark_read("acc1:INBOX:4", true).unwrap();
    }

    /// Finding 2 (CRITICAL): nothing serialised two rebuilds, and each one
    /// took the connection mutex separately to read, delete, write and
    /// clear -- so a second rebuild (performSync has no per-account
    /// in-flight guard: a retry, reSyncAll and a replay-triggered sync can
    /// overlap) could run in the first one's gaps: `UNIQUE constraint
    /// failed: threads.id`, or a projection different from a full rebuild.
    /// A second rebuild started while the first is paused in ANY of its
    /// gaps must wait for the first to finish.
    #[test]
    fn a_second_rebuild_waits_for_the_first_instead_of_interleaving() {
        use std::cell::{Cell, RefCell};
        use std::rc::Rc;
        use std::sync::Arc;
        let db = Arc::new(fresh_db());
        seed_merge_scenario(&db);

        type Handle = std::thread::JoinHandle<Result<(), DbError>>;
        let second: Rc<RefCell<Option<Handle>>> = Rc::new(RefCell::new(None));
        let finished_inside_gap = Rc::new(Cell::new(None::<bool>));
        let hook = {
            let db = Arc::clone(&db);
            let second = Rc::clone(&second);
            let finished_inside_gap = Rc::clone(&finished_inside_gap);
            move |_: &Database, _phase: &'static str| {
                if second.borrow().is_some() {
                    return;
                }
                let db = Arc::clone(&db);
                let handle =
                    std::thread::spawn(move || db.rebuild_threads_incremental("acc1").map(|_| ()));
                // Plenty of time for a rebuild of five messages to finish
                // if nothing makes it wait.
                std::thread::sleep(std::time::Duration::from_millis(300));
                finished_inside_gap.set(Some(handle.is_finished()));
                *second.borrow_mut() = Some(handle);
            }
        };
        let first = with_rebuild_gap_hook(hook, || db.rebuild_threads_incremental("acc1"));
        let second = second.borrow_mut().take().expect("the hook fired");
        let second = second.join().expect("second rebuild did not panic");

        assert!(first.is_ok(), "first rebuild: {first:?}");
        assert!(second.is_ok(), "second rebuild: {second:?}");
        assert_eq!(
            finished_inside_gap.get(),
            Some(false),
            "a second rebuild ran to completion inside the first one's gap"
        );
        assert_eq!(
            actual_projection(&db, "acc1"),
            expected_projection(&db, "acc1")
        );
        assert_eq!(dirty_row_count(&db, "acc1"), 0);
    }

    /// The writes the finding-3 test lands inside a rebuild.
    #[derive(Clone, Copy, Debug)]
    enum MidRebuildWrite {
        /// mark_read on the dirty seed itself (the lost-write case).
        FlipSeedRead,
        /// mark_read on a thread member that is not a seed.
        FlipOtherMemberRead,
        /// Re-hydration of the seed (the thread root) with a new subject.
        HydrateSeedSubject,
        /// Re-hydration of the seed adding a Reference into another
        /// conversation (a merge).
        HydrateSeedMergesThreads,
        /// A new reply to the conversation.
        NewReply,
        /// A new message dated before the root, which becomes the root: the
        /// thread id changes.
        NewEarlierRoot,
        /// Move a member to Trash (the local row leaves).
        DeleteMember,
    }

    fn perform_mid_rebuild_write(db: &Database, write: MidRebuildWrite) {
        match write {
            MidRebuildWrite::FlipSeedRead => db.mark_read("acc1:INBOX:1", false).unwrap(),
            MidRebuildWrite::FlipOtherMemberRead => db.mark_read("acc1:INBOX:3", true).unwrap(),
            MidRebuildWrite::HydrateSeedSubject => {
                let mut m = msg(1, "Plans (corrected)", 1_000);
                m.attachments = Some(vec![]);
                db.upsert_message(&m).unwrap();
            }
            MidRebuildWrite::HydrateSeedMergesThreads => {
                let mut m = msg(1, "Plans", 1_000);
                m.attachments = Some(vec![]);
                m.references = vec!["<4@local.test>".into()];
                db.upsert_message(&m).unwrap();
            }
            MidRebuildWrite::NewReply => {
                let mut m = msg(10, "Re: Plans", 5_000);
                m.references = vec!["<3@local.test>".into()];
                db.upsert_message(&m).unwrap();
            }
            MidRebuildWrite::NewEarlierRoot => {
                let mut m = msg(11, "Before plans", 10);
                m.references = vec!["<1@local.test>".into()];
                db.upsert_message(&m).unwrap();
            }
            MidRebuildWrite::DeleteMember => db.delete_message("acc1:INBOX:2").unwrap(),
        }
    }

    /// Finding 3: a write to an ALREADY-dirty message while a rebuild is in
    /// flight was lost -- `INSERT OR IGNORE` left the existing dirty row as
    /// it was, and the rebuild's final clear deleted every seed it started
    /// from, so the aggregate stayed stale for good. Every write kind, at
    /// every point a rebuild lets go of the mutex, followed by one quiescent
    /// pass, must end at exactly the full-rebuild projection with nothing
    /// left dirty.
    #[test]
    fn a_write_that_lands_mid_rebuild_is_never_lost() {
        use std::cell::Cell;
        use std::rc::Rc;
        let writes = [
            MidRebuildWrite::FlipSeedRead,
            MidRebuildWrite::FlipOtherMemberRead,
            MidRebuildWrite::HydrateSeedSubject,
            MidRebuildWrite::HydrateSeedMergesThreads,
            MidRebuildWrite::NewReply,
            MidRebuildWrite::NewEarlierRoot,
            MidRebuildWrite::DeleteMember,
        ];
        for write in writes {
            for target_gap in 0.. {
                let db = fresh_db();
                seed_account_mailbox(&db);
                // Conversation 1 <- 2 <- 3, plus a singleton 4.
                let a = msg(1, "Plans", 1_000);
                let mut b = msg(2, "Re: Plans", 2_000);
                b.references = vec!["<1@local.test>".into()];
                let mut c = msg(3, "Re: Re: Plans", 3_000);
                c.references = vec!["<2@local.test>".into()];
                let d = msg(4, "Elsewhere", 4_000);
                for m in [&a, &b, &c, &d] {
                    db.upsert_message(m).unwrap();
                }
                pad_with_unrelated(&db, 40);
                db.rebuild_threads("acc1").unwrap();
                // The seed: the root, read.
                db.mark_read("acc1:INBOX:1", true).unwrap();

                let gaps = Rc::new(Cell::new(0usize));
                let fired = Rc::new(Cell::new(false));
                let hook = {
                    let gaps = Rc::clone(&gaps);
                    let fired = Rc::clone(&fired);
                    move |db: &Database, _phase: &'static str| {
                        let n = gaps.get();
                        gaps.set(n + 1);
                        if n == target_gap {
                            perform_mid_rebuild_write(db, write);
                            fired.set(true);
                        }
                    }
                };
                with_rebuild_gap_hook(hook, || db.rebuild_threads_incremental("acc1")).unwrap();
                // One quiescent pass picks up whatever the write re-dirtied.
                db.rebuild_threads_incremental("acc1").unwrap();

                let ctx = format!("{write:?} at gap {target_gap}");
                assert_eq!(
                    actual_projection(&db, "acc1"),
                    expected_projection(&db, "acc1"),
                    "{ctx}: a write that landed mid-rebuild was lost"
                );
                assert_eq!(dirty_row_count(&db, "acc1"), 0, "{ctx}");
                assert!(!db.threads_dirty("acc1").unwrap(), "{ctx}");
                if !fired.get() {
                    // No gap with this index: every interleaving covered.
                    assert!(target_gap > 0, "the rebuild never let go of the mutex");
                    break;
                }
            }
        }
    }

    /// Finding 4: old thread rows used to be deleted and COMMITTED before
    /// the new ones were written, so a reader landing in between saw
    /// messages whose `thread_id` had no row (`list_thread_summaries` drops
    /// them). At every point a rebuild lets go of the mutex, a reader must
    /// see no dangling `thread_id` and no memberless thread row -- whether
    /// the thread keeps its id (a read flip), changes it (a new earlier
    /// root) or merges with another (a reply referencing both).
    #[test]
    fn readers_never_see_a_message_whose_thread_row_is_missing() {
        use std::cell::RefCell;
        use std::rc::Rc;
        for scenario in ["read_flip", "new_root", "merge"] {
            let db = fresh_db();
            seed_account_mailbox(&db);
            let a = msg(1, "A", 1_000);
            let mut a2 = msg(2, "Re: A", 2_000);
            a2.references = vec!["<1@local.test>".into()];
            let b = msg(3, "B", 3_000);
            for m in [&a, &a2, &b] {
                db.upsert_message(m).unwrap();
            }
            pad_with_unrelated(&db, 40);
            db.rebuild_threads("acc1").unwrap();
            match scenario {
                "read_flip" => db.mark_read("acc1:INBOX:2", true).unwrap(),
                "new_root" => {
                    let mut z = msg(9, "Earlier", 10);
                    z.references = vec!["<1@local.test>".into()];
                    db.upsert_message(&z).unwrap();
                }
                _ => {
                    let mut m = msg(9, "Re: A and B", 4_000);
                    m.references = vec!["<1@local.test>".into(), "<3@local.test>".into()];
                    db.upsert_message(&m).unwrap();
                }
            }

            let violations: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
            let hook = {
                let violations = Rc::clone(&violations);
                move |db: &Database, phase: &'static str| {
                    let dangling = dangling_thread_ids(db, "acc1");
                    let memberless = memberless_threads(db, "acc1");
                    if dangling != 0 || memberless != 0 {
                        violations.borrow_mut().push(format!(
                            "{phase}: {dangling} dangling thread_id(s), {memberless} memberless thread(s)"
                        ));
                    }
                }
            };
            with_rebuild_gap_hook(hook, || db.rebuild_threads_incremental("acc1")).unwrap();
            assert!(
                violations.borrow().is_empty(),
                "{scenario}: {:?}",
                violations.borrow()
            );
            assert_eq!(
                actual_projection(&db, "acc1"),
                expected_projection(&db, "acc1"),
                "{scenario}"
            );
        }
    }

    /// Found while strengthening the property test: re-hydration that
    /// CHANGES an existing Message-ID (not just fills in a missing one --
    ///'s case) regroups the thread too, since replies citing
    /// the old id no longer resolve to this message. Under the incremental
    /// engine an unrecorded change is stale for good, not just until the
    /// next full rebuild.
    #[test]
    fn a_changed_message_id_regroups_its_thread() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let a = msg(1, "Root", 1_000);
        let mut b = msg(2, "Re: Root", 2_000);
        b.references = vec!["<1@local.test>".into()];
        db.upsert_message(&a).unwrap();
        db.upsert_message(&b).unwrap();
        pad_with_unrelated(&db, 40);
        db.rebuild_threads("acc1").unwrap();

        let mut rehydrated = msg(1, "Root", 1_000);
        rehydrated.rfc822_message_id = Some("<1-normalised@local.test>".into());
        db.upsert_message(&rehydrated).unwrap();
        assert!(matches!(
            db.rebuild_threads_incremental("acc1").unwrap(),
            ThreadRebuildPath::Incremental { .. }
        ));
        assert_eq!(
            actual_projection(&db, "acc1"),
            expected_projection(&db, "acc1")
        );
    }

    /// (pre-existing, fixed with finding 4): `thread_extractions`
    /// is `REFERENCES threads(id) ON DELETE CASCADE`, and both rebuild paths
    /// deleted and re-inserted every thread row they touched -- wiping a
    /// thread's AI extractions on every sync that touched it, even with its
    /// id unchanged. Rebuilds now UPDATE a surviving id in place and delete
    /// only ids that genuinely disappeared (where the cascade is correct: a
    /// new earlier root renames the thread).
    #[test]
    fn a_rebuild_keeps_ai_extractions_for_a_thread_whose_id_survives() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let a = msg(1, "Invoice", 1_000);
        let mut b = msg(2, "Re: Invoice", 2_000);
        b.references = vec!["<1@local.test>".into()];
        db.upsert_message(&a).unwrap();
        db.upsert_message(&b).unwrap();
        pad_with_unrelated(&db, 40);
        db.rebuild_threads("acc1").unwrap();
        let thread_id = "t:acc1:INBOX:1";
        db.replace_thread_extractions(
            thread_id,
            &[crate::NewThreadExtraction {
                message_id: None,
                kind: "action_item",
                text: "Pay the invoice",
                structured_value: None,
            }],
        )
        .unwrap();

        // Aggregate change only (incremental).
        db.mark_read("acc1:INBOX:2", true).unwrap();
        db.rebuild_threads_incremental("acc1").unwrap();
        assert_eq!(db.get_thread_extractions(thread_id).unwrap().len(), 1);
        // The thread grows (incremental), id unchanged.
        let mut c = msg(3, "Re: Re: Invoice", 3_000);
        c.references = vec!["<2@local.test>".into()];
        db.upsert_message(&c).unwrap();
        db.rebuild_threads_incremental("acc1").unwrap();
        assert_eq!(db.get_thread_extractions(thread_id).unwrap().len(), 1);
        // A full rebuild keeps it too.
        db.rebuild_threads("acc1").unwrap();
        assert_eq!(db.get_thread_extractions(thread_id).unwrap().len(), 1);
        assert_eq!(
            actual_projection(&db, "acc1"),
            expected_projection(&db, "acc1")
        );

        // A new earlier root renames the thread: the old id is gone, but
        // its extraction now follows the old root message
        // (id 1) to the thread that absorbed it, instead of being cascaded
        // away with the old id.
        let mut z = msg(9, "Earlier", 10);
        z.references = vec!["<1@local.test>".into()];
        db.upsert_message(&z).unwrap();
        db.rebuild_threads_incremental("acc1").unwrap();
        let new_thread_id = "t:acc1:INBOX:9";
        assert!(db.get_thread_extractions(thread_id).unwrap().is_empty());
        let moved = db.get_thread_extractions(new_thread_id).unwrap();
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].text, "Pay the invoice");
        assert_eq!(
            actual_projection(&db, "acc1"),
            expected_projection(&db, "acc1")
        );

        // A full rebuild from this same state must agree.
        db.rebuild_threads("acc1").unwrap();
        assert_eq!(db.get_thread_extractions(new_thread_id).unwrap().len(), 1);
    }

    ///: a merge that leaves BOTH sides' old thread ids naming
    /// their own AI extraction job must keep exactly one -- the newer job,
    /// wholesale (there is no per-item identity to interleave two runs'
    /// rows under one thread id, same reasoning as
    /// `replace_thread_extractions`). Here thread A survives in place (it
    /// stays the root) while thread B's id is swept, so this also proves
    /// the repoint runs even when the surviving id was never "gone".
    #[test]
    fn a_root_changing_merge_keeps_the_newer_ai_extraction() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "A", 1_000)).unwrap();
        db.upsert_message(&msg(2, "B", 1_500)).unwrap();
        pad_with_unrelated(&db, 40);
        db.rebuild_threads("acc1").unwrap();
        let thread_a = "t:acc1:INBOX:1";
        let thread_b = "t:acc1:INBOX:2";

        db.replace_thread_extractions(
            thread_a,
            &[crate::NewThreadExtraction {
                message_id: None,
                kind: "action_item",
                text: "From A",
                structured_value: None,
            }],
        )
        .unwrap();
        db.replace_thread_extractions(
            thread_b,
            &[crate::NewThreadExtraction {
                message_id: None,
                kind: "action_item",
                text: "From B",
                structured_value: None,
            }],
        )
        .unwrap();
        // Pin an explicit, deterministic ordering instead of trusting two
        // back-to-back `datetime('now')` calls to land in different
        // seconds: B's job is the newer one.
        db.lock_conn()
            .execute(
                "UPDATE thread_extractions SET created_at = '2026-01-01 00:00:00' \
                 WHERE thread_id = ?1",
                params![thread_a],
            )
            .unwrap();
        db.lock_conn()
            .execute(
                "UPDATE thread_extractions SET created_at = '2026-01-02 00:00:00' \
                 WHERE thread_id = ?1",
                params![thread_b],
            )
            .unwrap();

        // B now cites A: same root (A, the earlier message) so thread_a's
        // id is unchanged, but thread_b disappears into it. `attachments`
        // must be `Some` (an authoritative hydration) for a re-upsert of an
        // EXISTING message to actually rewrite `message_references` --
        // see `upsert_message_tx`'s `attachments_are_authoritative` gate.
        let mut b = msg(2, "B", 1_500);
        b.references = vec!["<1@local.test>".into()];
        b.attachments = Some(vec![]);
        db.upsert_message(&b).unwrap();
        db.rebuild_threads_incremental("acc1").unwrap();

        assert!(db.get_thread_extractions(thread_b).unwrap().is_empty());
        let survivor = db.get_thread_extractions(thread_a).unwrap();
        assert_eq!(survivor.len(), 1, "exactly one job survives the merge");
        assert_eq!(
            survivor[0].text, "From B",
            "the newer job (B) wins, not A's stale one"
        );
        assert_eq!(
            actual_projection(&db, "acc1"),
            expected_projection(&db, "acc1")
        );

        // The full-rebuild path must agree with the incremental one.
        db.rebuild_threads("acc1").unwrap();
        let after_full = db.get_thread_extractions(thread_a).unwrap();
        assert_eq!(after_full.len(), 1);
        assert_eq!(after_full[0].text, "From B");
    }

    ///: a regroup that turns one old thread id into SEVERAL
    /// new ones (a split) must land the extraction on the specific new
    /// thread that contains the OLD ROOT MESSAGE -- not just any former
    /// member of the old thread. Here the old thread {1, 2, 3} (root 1)
    /// splits: message 3 breaks off on its own, while a new, earlier
    /// message (9) joins {1, 2} and renames their thread. Message 3 was
    /// also a former member of the old id, so a naive "any surviving
    /// member" fallback would wrongly hand it the extraction instead.
    #[test]
    fn a_root_changing_split_keeps_extraction_on_the_old_roots_thread() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let a = msg(1, "Root", 1_000);
        let mut b = msg(2, "Re: Root", 2_000);
        b.references = vec!["<1@local.test>".into()];
        let mut c = msg(3, "Re: Re: Root", 3_000);
        c.references = vec!["<2@local.test>".into()];
        db.upsert_message(&a).unwrap();
        db.upsert_message(&b).unwrap();
        db.upsert_message(&c).unwrap();
        pad_with_unrelated(&db, 40);
        db.rebuild_threads("acc1").unwrap();
        let old_thread_id = "t:acc1:INBOX:1";
        assert_eq!(
            db.get_thread_extractions(old_thread_id).unwrap().len(),
            0,
            "sanity: nothing yet"
        );
        db.replace_thread_extractions(
            old_thread_id,
            &[crate::NewThreadExtraction {
                message_id: None,
                kind: "date",
                text: "Meeting Friday",
                structured_value: Some(r#"{"date":"2026-09-25"}"#),
            }],
        )
        .unwrap();

        // C breaks off (its References no longer resolve to anything we
        // have), and Z -- earlier than the old root -- joins {1, 2}.
        // `attachments: Some(vec![])` makes this an authoritative
        // hydration so the new References actually get written (see
        // `upsert_message_tx`'s `attachments_are_authoritative` gate).
        let mut c_detached = msg(3, "Re: Re: Root", 3_000);
        c_detached.references = vec!["<nonexistent@local.test>".into()];
        c_detached.attachments = Some(vec![]);
        db.upsert_message(&c_detached).unwrap();
        let mut z = msg(9, "Earlier still", 10);
        z.references = vec!["<1@local.test>".into()];
        db.upsert_message(&z).unwrap();
        db.rebuild_threads_incremental("acc1").unwrap();

        let new_root_thread = "t:acc1:INBOX:9";
        let detached_thread = "t:acc1:INBOX:3";
        assert!(db.get_thread_extractions(old_thread_id).unwrap().is_empty());
        assert!(
            db.get_thread_extractions(detached_thread)
                .unwrap()
                .is_empty(),
            "the split-off member must NOT inherit the extraction"
        );
        let moved = db.get_thread_extractions(new_root_thread).unwrap();
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].text, "Meeting Friday");
        assert_eq!(
            actual_projection(&db, "acc1"),
            expected_projection(&db, "acc1")
        );

        // The full-rebuild path must agree.
        db.rebuild_threads("acc1").unwrap();
        assert_eq!(db.get_thread_extractions(new_root_thread).unwrap().len(), 1);
        assert!(db
            .get_thread_extractions(detached_thread)
            .unwrap()
            .is_empty());
    }

    /// #3445/#3458: a conversation id is `t:<root message id>`, so an earlier
    /// message arriving changes it. Links (either endpoint), suggestions and
    /// per-thread AI outputs keyed by the old id must follow the thread, on
    /// the incremental AND the full path.
    #[test]
    fn a_root_change_carries_links_suggestions_and_ai_outputs_to_the_new_id() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let a = msg(1, "Root", 1_000);
        let mut b = msg(2, "Re: Root", 2_000);
        b.references = vec!["<1@local.test>".into()];
        db.upsert_message(&a).unwrap();
        db.upsert_message(&b).unwrap();
        pad_with_unrelated(&db, 40);
        db.rebuild_threads("acc1").unwrap();
        let old = "t:acc1:INBOX:1";
        {
            let c = db.lock_conn();
            c.execute(
                "INSERT INTO links (src_type, src_id, dst_type, dst_id, relation, origin,
                                    created_at, updated_at)
                 VALUES ('project', 'p1', 'thread', ?1, 'includes', 'manual', 1, 1),
                        ('thread', ?1, 'work_item', 'w1', 'about', 'manual', 1, 1)",
                params![old],
            )
            .unwrap();
            c.execute(
                "INSERT INTO suggestions (id, kind, payload_json, thread_id, origin,
                                          dedupe_key, created_at)
                 VALUES ('s1', 'waiting_for_reply', '{}', ?1, 'heuristic', 'k1', 1)",
                params![old],
            )
            .unwrap();
            c.execute(
                "INSERT INTO ai_outputs (job_id, scope, owner_id, feature, text)
                 VALUES (1, 'thread', ?1, 'summary', 'x')",
                params![old],
            )
            .unwrap();
        }

        // An earlier message joins the conversation: the root becomes 9.
        let mut z = msg(9, "Earlier still", 10);
        z.references = vec!["<1@local.test>".into()];
        db.upsert_message(&z).unwrap();
        db.rebuild_threads_incremental("acc1").unwrap();
        let new = "t:acc1:INBOX:9";
        let count = |sql: &str, id: &str| -> i64 {
            db.lock_conn()
                .query_row(sql, params![id], |r| r.get(0))
                .unwrap()
        };
        let links_on = "SELECT COUNT(*) FROM links WHERE (src_type='thread' AND src_id=?1) \
                        OR (dst_type='thread' AND dst_id=?1)";
        assert_eq!(count(links_on, old), 0, "no link may keep the dead id");
        assert_eq!(count(links_on, new), 2, "both links follow the thread");
        assert_eq!(
            count("SELECT COUNT(*) FROM suggestions WHERE thread_id = ?1", new),
            1
        );
        assert_eq!(
            count(
                "SELECT COUNT(*) FROM ai_outputs WHERE scope='thread' AND owner_id = ?1",
                new
            ),
            1
        );
    }

    /// (PR #183 review): when the old root message has itself
    /// departed, `repoint_thread_extractions`'s fallback (no literal root to
    /// anchor on) must pick the earliest-by-`(sent_at, id)` former member,
    /// never "whichever `msgs` happened to list first" -- unhinted, that row
    /// order is the id's plain TEXT order, where "acc1:INBOX:10" sorts
    /// before "acc1:INBOX:9". Message 9 (sent_at 300) must win over message
    /// 10 (sent_at 900) despite message 10's smaller text id, and it must
    /// win the SAME way whether the closure this pass is one chunk or many
    /// (full rebuild's `msgs` is the whole account; an incremental chunk's
    /// is a `HashSet`-ordered batch) -- deterministic, not row-order-lucky.
    #[test]
    fn repoint_fallback_prefers_earliest_sent_at_not_row_order() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let root = msg(1, "Root", 100);
        let mut nine = msg(9, "Reply nine", 300);
        nine.references = vec!["<1@local.test>".into()];
        let mut ten = msg(10, "Reply ten", 900);
        ten.references = vec!["<1@local.test>".into()];
        db.upsert_message(&root).unwrap();
        db.upsert_message(&nine).unwrap();
        db.upsert_message(&ten).unwrap();
        pad_with_unrelated(&db, 40);
        db.rebuild_threads("acc1").unwrap();
        let old_thread_id = "t:acc1:INBOX:1";
        db.replace_thread_extractions(
            old_thread_id,
            &[crate::NewThreadExtraction {
                message_id: None,
                kind: "action_item",
                text: "Follow up",
                structured_value: None,
            }],
        )
        .unwrap();

        // The root departs. Its rfc822 id no longer resolves to anything,
        // so 9 and 10 each become their OWN standalone thread -- but the
        // old thread's extraction must still move as a whole to exactly
        // one of them: the earlier-sent_at survivor (9), not the
        // smaller-text-id one (10).
        db.purge_message("acc1", "acc1:INBOX:1").unwrap();
        db.rebuild_threads_incremental("acc1").unwrap();

        let earlier_survivor = "t:acc1:INBOX:9";
        let later_survivor = "t:acc1:INBOX:10";
        assert!(db.get_thread_extractions(old_thread_id).unwrap().is_empty());
        assert!(
            db.get_thread_extractions(later_survivor)
                .unwrap()
                .is_empty(),
            "the later-sent_at survivor (smaller text id) must NOT get the extraction"
        );
        let moved = db.get_thread_extractions(earlier_survivor).unwrap();
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].text, "Follow up");
        assert_eq!(
            actual_projection(&db, "acc1"),
            expected_projection(&db, "acc1")
        );

        // The full-rebuild path must agree.
        db.rebuild_threads("acc1").unwrap();
        assert_eq!(
            db.get_thread_extractions(earlier_survivor).unwrap().len(),
            1
        );
        assert!(db
            .get_thread_extractions(later_survivor)
            .unwrap()
            .is_empty());
    }

    /// Findings 2 + 3 under real scheduling: two threads rebuilding in a
    /// loop while a third flips read flags (local and sync-reconciled) and
    /// re-hydrates messages (subject and References changes, merges and
    /// splits). No rebuild may error, and once the writer stops, one
    /// quiescent pass must reach exactly the full-rebuild projection with
    /// nothing dirty. Many seeded trials; the review's scratch version of
    /// this failed 192 of 200 against 8955fb78.
    #[test]
    fn concurrent_rebuilds_and_writes_converge_to_a_full_rebuild() {
        use std::sync::atomic::AtomicBool;
        const TRIALS: u64 = 40;
        const MESSAGES: i64 = 40;
        for trial in 0..TRIALS {
            let db = fresh_db();
            seed_account_mailbox(&db);
            let mut rng = SplitMix(0x5EED_0000 + trial);
            for uid in 1..=MESSAGES {
                let mut m = msg(uid, &format!("S{uid}"), 1_000 + uid * 10);
                if uid > 1 && rng.range(3) != 0 {
                    let parent = 1 + rng.range((uid - 1) as usize) as i64;
                    m.references = vec![format!("<{parent}@local.test>")];
                }
                db.upsert_message(&m).unwrap();
            }
            pad_with_unrelated(&db, 160);
            db.rebuild_threads("acc1").unwrap();

            let done = AtomicBool::new(false);
            let errors: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
            std::thread::scope(|s| {
                for _ in 0..2 {
                    s.spawn(|| {
                        while !done.load(Ordering::Acquire) {
                            if let Err(e) = db.rebuild_threads_incremental("acc1") {
                                errors.lock().unwrap().push(e.to_string());
                            }
                            std::thread::yield_now();
                        }
                    });
                }
                s.spawn(|| {
                    let mut rng = SplitMix(0xF00D_0000 + trial);
                    for op in 0..60 {
                        let uid = 1 + rng.range(MESSAGES as usize) as i64;
                        let id = format!("acc1:INBOX:{uid}");
                        let result = match rng.range(4) {
                            0 => db.mark_read(&id, rng.bool()),
                            1 => db.reconcile_message_flags("acc1", &id, rng.bool(), false),
                            _ => {
                                let mut m = msg(uid, &format!("S{uid} v{op}"), 1_000 + uid * 10);
                                m.attachments = Some(vec![]);
                                if rng.range(3) != 0 {
                                    let other = 1 + rng.range(MESSAGES as usize) as i64;
                                    if other != uid {
                                        m.references = vec![format!("<{other}@local.test>")];
                                    }
                                }
                                db.upsert_message(&m)
                            }
                        };
                        if let Err(e) = result {
                            errors.lock().unwrap().push(format!("writer: {e}"));
                        }
                        if op % 8 == 0 {
                            std::thread::yield_now();
                        }
                    }
                    done.store(true, Ordering::Release);
                });
            });

            let errors = errors.into_inner().unwrap();
            assert!(errors.is_empty(), "trial {trial}: {errors:?}");
            db.rebuild_threads_incremental("acc1").unwrap();
            assert_eq!(
                actual_projection(&db, "acc1"),
                expected_projection(&db, "acc1"),
                "trial {trial}: concurrent rebuilds + writes diverged from a full rebuild"
            );
            assert_eq!(dirty_row_count(&db, "acc1"), 0, "trial {trial}");
            assert_eq!(dangling_thread_ids(&db, "acc1"), 0, "trial {trial}");
            assert_eq!(memberless_threads(&db, "acc1"), 0, "trial {trial}");
        }
    }

    /// Verified review finding: body hydration on an existing descriptor-only
    /// row deletes and re-inserts `message_references` with the full
    /// References chain, which can change thread grouping -- but sync-time
    /// hydration passes fetch 0 new messages, so sync.rs's
    /// `any_remote_change` never sees it, and standalone hydrators never call
    /// sync.rs at all. `upsert_message` must mark the account dirty itself
    /// when this happens, so a later sync's rebuild-skip decision (S64)
    /// still regroups the thread.
    #[test]
    fn hydration_that_adds_a_reference_sets_threads_dirty_and_regroups_the_thread() {
        let db = fresh_db();
        seed_account_mailbox(&db);

        // Two unrelated descriptor-only messages: no References header seen
        // yet (attachments: None, i.e. not yet hydrated).
        db.upsert_message(&threaded_msg(1, "Root", 1000, "<root@x>", &[]))
            .unwrap();
        db.upsert_message(&threaded_msg(2, "Re: Root", 2000, "<reply@x>", &[]))
            .unwrap();
        db.rebuild_threads("acc1").unwrap();
        assert!(
            !db.threads_dirty("acc1").unwrap(),
            "rebuild_threads clears the marker"
        );
        assert_eq!(
            db.list_thread_summaries("acc1", 50, 0).unwrap().len(),
            2,
            "no References link yet -> two separate threads"
        );

        // Hydration: the full body parse for message 2 discovers it actually
        // references message 1 (a real Reply). attachments: Some(vec![])
        // marks this upsert as authoritative for addresses/references, same
        // as a real body hydration with no attachments.
        let mut hydrated = threaded_msg(2, "Re: Root", 2000, "<reply@x>", &["<root@x>"]);
        hydrated.attachments = Some(vec![]);
        db.upsert_message(&hydrated).unwrap();

        assert!(
            db.threads_dirty("acc1").unwrap(),
            "a hydration that changes the References chain on an existing \
             row must mark the account dirty -- sync never observes this as \
             a remote change"
        );

        db.rebuild_threads("acc1").unwrap();
        assert!(!db.threads_dirty("acc1").unwrap(), "rebuild clears it");
        assert_eq!(
            db.list_thread_summaries("acc1", 50, 0).unwrap().len(),
            1,
            "the new Reference link must regroup the two messages into one thread"
        );

        // A re-hydration with the SAME references must NOT mark dirty --
        // nothing about thread membership changed.
        let mut same_refs = threaded_msg(2, "Re: Root", 2000, "<reply@x>", &["<root@x>"]);
        same_refs.attachments = Some(vec![]);
        db.upsert_message(&same_refs).unwrap();
        assert!(
            !db.threads_dirty("acc1").unwrap(),
            "an unchanged References chain must not force an extra rebuild"
        );
    }

    ///: an envelope with no Message-ID gets `rfc822_message_id`
    /// filled in on hydration, which can regroup the thread (a later
    /// reply's References may point at this filled-in id) -- exactly the
    /// same blind spot as a References-chain change, so `upsert_message`
    /// must mark dirty here too.
    #[test]
    fn hydration_filling_a_null_message_id_marks_threads_dirty() {
        let db = fresh_db();
        seed_account_mailbox(&db);

        // A descriptor-only import with no Message-ID header seen yet.
        let mut no_msgid = msg(1, "No header yet", 1000);
        no_msgid.rfc822_message_id = None;
        db.upsert_message(&no_msgid).unwrap();
        db.rebuild_threads("acc1").unwrap();
        assert!(!db.threads_dirty("acc1").unwrap());

        // Hydration discovers the real Message-ID.
        let mut hydrated = msg(1, "No header yet", 1000);
        hydrated.rfc822_message_id = Some("<found@x>".into());
        hydrated.attachments = Some(vec![]);
        db.upsert_message(&hydrated).unwrap();
        assert!(
            db.threads_dirty("acc1").unwrap(),
            "filling a previously-null rfc822_message_id can regroup the \
             thread -- sync never observes this as a remote change"
        );
        db.rebuild_threads("acc1").unwrap();

        // A further re-hydration with the SAME (now non-null) id must NOT
        // mark dirty again.
        let mut same = msg(1, "No header yet", 1000);
        same.rfc822_message_id = Some("<found@x>".into());
        same.attachments = Some(vec![]);
        db.upsert_message(&same).unwrap();
        assert!(
            !db.threads_dirty("acc1").unwrap(),
            "an already-filled id that doesn't change must not force an extra rebuild"
        );
    }

    ///: `sent_at` moving from INTERNALDATE to the Date header
    /// on hydration changes a thread's first/last date, and so its id and
    /// subject/root selection -- another blind spot sync.rs never observes.
    #[test]
    fn hydration_changing_sent_at_marks_threads_dirty() {
        let db = fresh_db();
        seed_account_mailbox(&db);

        db.upsert_message(&msg(1, "Subject", 1000)).unwrap();
        db.rebuild_threads("acc1").unwrap();
        assert!(!db.threads_dirty("acc1").unwrap());

        let mut hydrated = msg(1, "Subject", 2000);
        hydrated.attachments = Some(vec![]);
        db.upsert_message(&hydrated).unwrap();
        assert!(
            db.threads_dirty("acc1").unwrap(),
            "sent_at changing on hydration can reorder/reroot a thread"
        );
        db.rebuild_threads("acc1").unwrap();

        // A re-hydration with the SAME sent_at must NOT mark dirty again.
        let mut same = msg(1, "Subject", 2000);
        same.attachments = Some(vec![]);
        db.upsert_message(&same).unwrap();
        assert!(
            !db.threads_dirty("acc1").unwrap(),
            "an unchanged sent_at must not force an extra rebuild"
        );
    }

    /// Verified review finding: a UIDVALIDITY reset to an EMPTY mailbox (or
    /// the legacy-recovery path) purges local rows via
    /// `delete_messages_for_mailbox` without reporting a remote change to
    /// sync.rs, so it must mark the account dirty itself.
    #[test]
    fn delete_messages_for_mailbox_sets_threads_dirty() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "Hi", 1000)).unwrap();
        db.rebuild_threads("acc1").unwrap();
        assert!(!db.threads_dirty("acc1").unwrap());

        db.delete_messages_for_mailbox("acc1", "acc1:INBOX")
            .unwrap();
        assert!(
            db.threads_dirty("acc1").unwrap(),
            "purging a mailbox's rows removed a thread member with no \
             remote change for sync.rs to observe"
        );
        db.rebuild_threads("acc1").unwrap();
        assert!(!db.threads_dirty("acc1").unwrap());

        // A no-op purge (nothing there to delete) must not mark dirty.
        db.delete_messages_for_mailbox("acc1", "acc1:INBOX")
            .unwrap();
        assert!(
            !db.threads_dirty("acc1").unwrap(),
            "an empty mailbox has nothing for a rebuild to fix"
        );
    }

    /// Verified review finding: `clear_sync_cursor` (the legacy empty_trash
    /// recovery path) deletes Trash rows without marking the account dirty.
    #[test]
    fn clear_sync_cursor_sets_threads_dirty() {
        let db = fresh_db();
        seed_account_mailbox(&db); // acc1:INBOX
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Trash".into(),
            account_id: "acc1".into(),
            path: "Trash".into(),
            name: "Trash".into(),
            role: Some("trash".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.set_sync_cursor(
            "acc1",
            "acc1:Trash",
            &SyncCursor {
                uid_validity: Some(5),
                uid_next: Some(3),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
        let mut stale = msg(1, "stale under the old UID space", 1000);
        stale.id = "acc1:Trash:1".into();
        stale.mailbox_id = "acc1:Trash".into();
        stale.rfc822_message_id = Some("<stale@x>".into());
        db.upsert_message(&stale).unwrap();
        db.rebuild_threads("acc1").unwrap();
        assert!(!db.threads_dirty("acc1").unwrap());

        db.clear_sync_cursor("acc1", "Trash").unwrap();
        assert!(
            db.threads_dirty("acc1").unwrap(),
            "clearing the cursor purged Trash rows with no remote change \
             for sync.rs to observe"
        );

        // A no-op clear (nothing there) must not mark dirty.
        db.rebuild_threads("acc1").unwrap();
        db.clear_sync_cursor("acc1", "Trash").unwrap();
        assert!(
            !db.threads_dirty("acc1").unwrap(),
            "nothing left to purge -- no rebuild is owed"
        );
    }

    #[test]
    fn threads_do_not_merge_on_subject_alone() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        // Same subject, no references between them -> must stay separate.
        db.upsert_message(&threaded_msg(1, "Weekly", 1000, "a@x", &[]))
            .unwrap();
        db.upsert_message(&threaded_msg(2, "Weekly", 2000, "b@x", &[]))
            .unwrap();
        db.rebuild_threads("acc1").unwrap();
        assert_eq!(db.list_thread_summaries("acc1", 50, 0).unwrap().len(), 2);
    }

    #[test]
    fn duplicate_message_id_copies_share_a_thread() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        // Two copies of the same message (same rfc822 id, different uid) plus a
        // reply referencing it -> one thread (Inbox+Sent-copy scenario).
        db.upsert_message(&threaded_msg(1, "Deal", 1000, "same@x", &[]))
            .unwrap();
        db.upsert_message(&threaded_msg(2, "Deal", 1000, "same@x", &[]))
            .unwrap();
        db.upsert_message(&threaded_msg(3, "Re: Deal", 2000, "reply@x", &["same@x"]))
            .unwrap();
        db.rebuild_threads("acc1").unwrap();
        let threads = db.list_thread_summaries("acc1", 50, 0).unwrap();
        assert_eq!(
            threads.len(),
            1,
            "duplicate-id copies + reply form one thread"
        );
        assert_eq!(
            threads[0].message_count, 2,
            "two copies of one Message-ID are one conversation entry (#3458)"
        );
    }

    /// #3458: one mail synced into two folders/labels must not read as a
    /// thread of 2 with 2 unread, and reading either copy reads the mail.
    #[test]
    fn copies_in_two_folders_count_once_and_reading_one_reads_the_mail() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&threaded_msg(1, "Deal", 1000, "same@x", &[]))
            .unwrap();
        db.upsert_message(&threaded_msg(2, "Deal", 1000, "same@x", &[]))
            .unwrap();
        db.rebuild_threads("acc1").unwrap();
        let t = db.list_thread_summaries("acc1", 50, 0).unwrap();
        assert_eq!((t[0].message_count, t[0].unread_count), (1, 1));
        let tid = t[0].id.clone();
        assert_eq!(db.get_thread_messages(&tid).unwrap().len(), 1);

        db.mark_read("acc1:INBOX:1", true).unwrap();
        db.rebuild_threads_incremental("acc1").unwrap();
        let t = db.list_thread_summaries("acc1", 50, 0).unwrap();
        assert_eq!((t[0].message_count, t[0].unread_count), (1, 0));
        let msgs = db.get_thread_messages(&tid).unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].is_read, "read if any copy is read");
    }

    /// #3458: when only the later-sorting copy has a hydrated body, that is
    /// the copy returned (body readers load the body by the returned id).
    #[test]
    fn the_copy_with_a_body_represents_a_deduplicated_message() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        // The helper hydrates a body by default; the earlier copy has none.
        let mut bare = threaded_msg(1, "Deal", 1000, "same@x", &[]);
        bare.text_body = None;
        db.upsert_message(&bare).unwrap();
        let mut hydrated = threaded_msg(2, "Deal", 1000, "same@x", &[]);
        hydrated.text_body = Some("the body".into());
        db.upsert_message(&hydrated).unwrap();
        db.rebuild_threads("acc1").unwrap();
        let tid = db.list_thread_summaries("acc1", 50, 0).unwrap()[0]
            .id
            .clone();
        let msgs = db.get_thread_messages(&tid).unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].has_body, "the hydrated copy must be the one kept");
        assert_eq!(msgs[0].id, "acc1:INBOX:2");
    }

    /// #3458: replies to a message we do not hold still share an ancestor.
    /// The incremental path must agree with the full one.
    #[test]
    fn replies_to_the_same_missing_message_share_a_thread() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&threaded_msg(1, "Re: Gone", 1000, "b@x", &["missing@x"]))
            .unwrap();
        db.upsert_message(&threaded_msg(2, "Re: Gone", 2000, "c@x", &["missing@x"]))
            .unwrap();
        db.upsert_message(&threaded_msg(3, "Other", 3000, "o@x", &["elsewhere@x"]))
            .unwrap();
        pad_with_unrelated(&db, 40);
        db.rebuild_threads("acc1").unwrap();
        let distinct = |ids: &str| -> i64 {
            db.lock_conn()
                .query_row(
                    &format!("SELECT COUNT(DISTINCT thread_id) FROM messages WHERE id IN ({ids})"),
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(
            distinct("'acc1:INBOX:1','acc1:INBOX:2'"),
            1,
            "full rebuild groups siblings of a missing parent"
        );

        // Incremental: a third sibling arrives later and must join them.
        db.upsert_message(&threaded_msg(4, "Re: Gone", 4000, "d@x", &["missing@x"]))
            .unwrap();
        db.rebuild_threads_incremental("acc1").unwrap();
        assert_eq!(
            actual_projection(&db, "acc1"),
            expected_projection(&db, "acc1")
        );
        assert_eq!(
            distinct("'acc1:INBOX:1','acc1:INBOX:2','acc1:INBOX:4'"),
            1,
            "incremental closure finds the other citers"
        );
    }

    #[test]
    fn header_thread_count_and_mailbox_counts() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&threaded_msg(1, "T", 1000, "root@x", &[]))
            .unwrap();
        db.upsert_message(&threaded_msg(2, "Re: T", 2000, "r1@x", &["root@x"]))
            .unwrap();
        db.rebuild_threads("acc1").unwrap();

        let headers = db
            .list_message_headers("acc1", "acc1:INBOX", 50, 0)
            .unwrap();
        assert_eq!(headers.len(), 2);
        assert!(
            headers.iter().all(|h| h.thread_count == 2),
            "both messages are in a 2-message thread"
        );

        let counts = db.mailbox_counts("acc1").unwrap();
        let inbox = counts
            .iter()
            .find(|c| c.mailbox_id == "acc1:INBOX")
            .unwrap();
        assert_eq!(inbox.total, 2);
        assert_eq!(inbox.unread, 2); // msg() defaults is_read = false
    }

    /// #3687: snoozed mail is subtracted per mailbox (not probed per message);
    /// a snoozed READ message must leave the unread count alone, a snoozed
    /// UNREAD one must take one off both totals.
    #[test]
    fn mailbox_counts_subtracts_snoozed_read_and_unread_separately() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        for (uid, t) in [(1, 1000), (2, 2000), (3, 3000), (4, 4000)] {
            db.upsert_message(&msg(uid, "m", t)).unwrap();
        }
        db.mark_read("acc1:INBOX:1", true).unwrap();
        db.mark_read("acc1:INBOX:2", true).unwrap();
        // Snooze one read (1) and one unread (3) message.
        db.snooze_message("acc1:INBOX:1", 9_999_999_999).unwrap();
        db.snooze_message("acc1:INBOX:3", 9_999_999_999).unwrap();
        let counts = db.mailbox_counts("acc1").unwrap();
        let inbox = counts
            .iter()
            .find(|c| c.mailbox_id == "acc1:INBOX")
            .unwrap();
        // Visible: 2 (read) and 4 (unread).
        assert_eq!((inbox.total, inbox.unread), (2, 1));
    }

    /// Bug-hunt smart-search: a mailbox that empties must report a {0,0} row
    /// (the Swift map only overwrites present keys, so a missing row left a
    /// stale badge), and snoozed mail (hidden from every list) must not count.
    #[test]
    fn mailbox_counts_reports_empty_mailboxes_and_excludes_snoozed() {
        fn get_counts<'a>(c: &'a [MailboxCounts], id: &str) -> &'a MailboxCounts {
            c.iter().find(|c| c.mailbox_id == id).unwrap()
        }
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Empty".into(),
            account_id: "acc1".into(),
            path: "Empty".into(),
            name: "Empty".into(),
            role: None,
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Snoozed".into(),
            account_id: "acc1".into(),
            path: "Snoozed".into(),
            name: "Snoozed".into(),
            role: None,
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.upsert_message(&msg(1, "one", 1000)).unwrap();
        db.upsert_message(&msg(2, "two", 2000)).unwrap();
        db.snooze_message("acc1:INBOX:1", 9_999_999_999).unwrap();
        let mut only = msg(3, "only", 3000);
        only.mailbox_id = "acc1:Snoozed".into();
        only.id = "acc1:Snoozed:3".into();
        db.upsert_message(&only).unwrap();
        db.snooze_message("acc1:Snoozed:3", 9_999_999_999).unwrap();

        let counts = db.mailbox_counts("acc1").unwrap();
        let all = get_counts(&counts, "acc1:Snoozed");
        assert_eq!(
            (all.total, all.unread),
            (0, 0),
            "all-snoozed mailbox keeps a row"
        );
        let get = |id: &str| counts.iter().find(|c| c.mailbox_id == id).unwrap();
        let inbox = get("acc1:INBOX");
        let listed = db
            .list_message_headers("acc1", "acc1:INBOX", 50, 0)
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(inbox.total, 1, "snoozed mail is not in the badge");
        assert_eq!(inbox.unread, 1);
        let empty = get("acc1:Empty");
        assert_eq!((empty.total, empty.unread), (0, 0));
    }

    /// Bug-hunt smart-search: search results in Sent/Drafts carry the
    /// recipients like every list path does.
    #[test]
    fn search_results_in_sent_carry_recipients() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Sent".into(),
            account_id: "acc1".into(),
            path: "Sent".into(),
            name: "Sent".into(),
            role: Some("sent".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        let mut m = msg(5, "budget", 2000);
        m.id = "acc1:Sent:5".into();
        m.mailbox_id = "acc1:Sent".into();
        m.addresses = vec![
            Address {
                kind: "from".into(),
                name: None,
                address: "test@local.test".into(),
            },
            Address {
                kind: "to".into(),
                name: Some("Bob Builder".into()),
                address: "bob@example.com".into(),
            },
        ];
        db.upsert_message(&m).unwrap();
        let hits = db.search_messages("acc1", "budget", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].to_display.as_deref(), Some("Bob Builder"));
    }

    /// Bug-hunt smart-search: group/rule text conditions fold case and
    /// accents exactly like the search bar's `subject:` / `from:`.
    #[test]
    fn group_text_conditions_fold_case_and_accents() {
        use castellum_models::{
            BoolOp, FilterCondition, FilterField, FilterGroup, FilterOperator, FilterValue,
            SmartMailboxScope,
        };
        let db = fresh_db();
        seed_account_mailbox(&db);
        let mut m = msg(1, "CAFÉ menu", 1000);
        m.from_name = Some("JOSÉ Martí".into());
        m.addresses[0].name = Some("JOSÉ Martí".into());
        db.upsert_message(&m).unwrap();
        let count = |field, value: &str| {
            let g = FilterGroup {
                op: BoolOp::And,
                conditions: vec![FilterCondition {
                    field,
                    operator: FilterOperator::Contains,
                    value: FilterValue::Text(value.into()),
                }],
                groups: vec![],
            };
            db.count_group_matches(&g, &SmartMailboxScope::default(), 100)
                .unwrap()
                .0
        };
        assert_eq!(count(FilterField::Subject, "café"), 1);
        assert_eq!(count(FilterField::Subject, "cafe"), 1);
        assert_eq!(count(FilterField::From, "jose"), 1);
        assert_eq!(count(FilterField::From, "MARTÍ"), 1);
        assert_eq!(count(FilterField::Subject, "tea"), 0);
    }

    /// Auto-mark-read differs from the explicit toggle ONLY in evaluation
    /// mode. On a writable account it must still queue the remote intent —
    /// otherwise opening a message would stop propagating \Seen to the server
    /// for everyone, and no other test would notice.
    #[test]
    fn auto_mark_read_still_enqueues_when_writes_are_enabled() {
        let db = fresh_db();
        seed_account_mailbox(&db); // writable
        db.upsert_message(&msg(1, "Hi", 1000)).unwrap();

        db.mark_read_auto("acc1:INBOX:1", true).unwrap();

        let headers = db
            .list_message_headers("acc1", "acc1:INBOX", 10, 0)
            .unwrap();
        assert!(headers[0].is_read, "local flag applied");
        let ops = db.pending_operations("acc1").unwrap();
        assert_eq!(ops.len(), 1, "auto-mark must still replay when writable");
        assert_eq!(ops[0].kind, "mark_read");
        assert!(ops[0].payload_json.contains("\"is_read\":true"));
    }

    #[test]
    fn mark_read_applies_locally_and_enqueues() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "Hi", 1000)).unwrap();
        let id = "acc1:INBOX:1";

        db.mark_read(id, true).unwrap();

        // Local flag applied immediately.
        let headers = db
            .list_message_headers("acc1", "acc1:INBOX", 10, 0)
            .unwrap();
        assert!(headers[0].is_read);

        // And a replayable op is queued.
        let ops = db.pending_operations("acc1").unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].kind, "mark_read");
        assert_eq!(ops[0].status, "queued");
        assert!(ops[0].payload_json.contains("\"is_read\":true"));
        assert!(ops[0].payload_json.contains("acc1:INBOX:1"));

        // Executor state machine: running (attempts++) then succeeded → not pending.
        db.set_operation_status(ops[0].id, "running", None).unwrap();
        db.set_operation_status(ops[0].id, "succeeded", None)
            .unwrap();
        assert!(db.pending_operations("acc1").unwrap().is_empty());

        // Unknown message id is rejected (no partial write).
        assert!(db.mark_read("acc1:INBOX:999", true).is_err());
        assert!(db.pending_operations("acc1").unwrap().is_empty());

        // Updating a stale/unknown op id is an error, not a silent no-op.
        assert!(db.set_operation_status(999_999, "succeeded", None).is_err());
    }

    #[test]
    fn terminal_statuses_are_immutable() {
        let db = fresh_db();
        seed_account_mailbox(&db); // acc1:INBOX
        db.upsert_message(&msg(1, "a", 1000)).unwrap();
        db.mark_read("acc1:INBOX:1", true).unwrap();
        let op = &db.pending_operations("acc1").unwrap()[0];

        // Race shape: executor marks the op running, then a UIDVALIDITY reset
        // cancels it, then the executor tries to record success. The cancel
        // must win; the executor's late write is a benign no-op.
        db.set_operation_status(op.id, "running", None).unwrap();
        db.cancel_operations_for_purged_mailbox("acc1", "acc1:INBOX", "INBOX", "cancelled: test")
            .unwrap();
        db.set_operation_status(op.id, "succeeded", None).unwrap();

        let status: String = db
            .lock_conn()
            .query_row(
                "SELECT status FROM operations WHERE id = ?1",
                params![op.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "cancelled");
    }

    #[test]
    fn manual_retry_resets_only_failed_ops_and_keeps_the_blob() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "a", 1000)).unwrap();
        db.mark_read("acc1:INBOX:1", true).unwrap();
        let op_id = db.pending_operations("acc1").unwrap()[0].id;
        db.lock_conn()
            .execute(
                "INSERT INTO operation_blobs (operation_id, bytes) VALUES (?1, ?2)",
                params![op_id, b"payload-bytes".as_slice()],
            )
            .unwrap();

        // queued → retry is a no-op (nothing to reset).
        assert!(!db.retry_operation(op_id).unwrap());

        // failed_permanent → retry resets to queued with a fresh budget and
        // the blob survives for the re-execution.
        db.set_operation_status(op_id, "running", None).unwrap();
        db.set_operation_status(op_id, "failed_permanent", Some("boom"))
            .unwrap();
        assert!(db.retry_operation(op_id).unwrap());
        let op = db
            .pending_operations("acc1")
            .unwrap()
            .into_iter()
            .find(|o| o.id == op_id)
            .expect("retried op is pending again");
        assert_eq!(op.status, "queued");
        assert_eq!(op.attempts, 0);
        assert_eq!(op.last_error, None);
        assert_eq!(
            db.operation_blob(op_id).unwrap().as_deref(),
            Some(b"payload-bytes".as_slice())
        );

        // running and terminal statuses are never reset.
        db.set_operation_status(op_id, "running", None).unwrap();
        assert!(!db.retry_operation(op_id).unwrap());
        db.set_operation_status(op_id, "succeeded", None).unwrap();
        assert!(!db.retry_operation(op_id).unwrap());

        // Unknown ids are an error, not a silent no-op.
        assert!(db.retry_operation(999_999).is_err());
    }

    #[test]
    fn operation_claim_refuses_terminal_ops_and_bumps_attempts() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "a", 1000)).unwrap();
        db.mark_read("acc1:INBOX:1", true).unwrap();
        let op_id = db.pending_operations("acc1").unwrap()[0].id;

        // Claiming a queued op succeeds and bumps attempts (executor start).
        assert!(db.begin_operation_attempt(op_id).unwrap());
        let op = db.pending_operations("acc1").unwrap()[0].clone();
        assert_eq!(op.status, "running");
        assert_eq!(op.attempts, 1);

        // A cancelled op refuses the claim — the interleaving where a user
        // cancels between the executor's snapshot and its claim.
        db.set_operation_status(op_id, "failed", Some("boom"))
            .unwrap();
        assert!(db.cancel_operation(op_id).unwrap());
        assert!(!db.begin_operation_attempt(op_id).unwrap());

        // Unknown ids error.
        assert!(db.begin_operation_attempt(999_999).is_err());
    }

    ///: the claim used to match `running`, so a second
    /// executor (a concurrent replay or send drain, or a retry over a stale
    /// snapshot) could claim an op that was still in flight and apply it
    /// twice. Only a crash survivor from an EARLIER launch may be re-claimed.
    #[test]
    fn operation_claim_refuses_an_op_this_launch_is_still_running() {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-claim-running-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);

        let op_id = {
            let db = Database::open(&path).expect("open");
            seed_account_mailbox(&db);
            db.upsert_message(&msg(1, "a", 1000)).unwrap();
            db.mark_read("acc1:INBOX:1", true).unwrap();
            let op_id = db.pending_operations("acc1").unwrap()[0].id;

            // Executor A claims the op; it is now in flight.
            assert!(db.begin_operation_attempt(op_id).unwrap());
            let running = db.pending_operations("acc1").unwrap()[0].clone();
            assert_eq!((running.status.as_str(), running.attempts), ("running", 1));

            // Executor B, whatever its snapshot saw, must not claim it too:
            // not the plain claim, not a CAS from the `running` it now sees,
            // and not a CAS from the stale `queued` it saw before A's claim.
            assert!(!db.begin_operation_attempt(op_id).unwrap());
            assert!(!db.begin_operation_attempt_from(op_id, "running").unwrap());
            assert!(!db.begin_operation_attempt_from(op_id, "queued").unwrap());
            let after = db.pending_operations("acc1").unwrap()[0].clone();
            assert_eq!(after.attempts, 1, "a refused claim must not bump attempts");

            // A's attempt fails; the op is claimable again (a later run), but a
            // stale `queued` snapshot still loses the compare-and-swap.
            db.set_operation_status(op_id, "failed", Some("boom"))
                .unwrap();
            assert!(!db.begin_operation_attempt_from(op_id, "queued").unwrap());
            assert!(db.begin_operation_attempt_from(op_id, "failed").unwrap());
            assert!(!db.begin_operation_attempt(op_id).unwrap());
            op_id
        }; // the app dies with the op `running` (attempt 2 in flight)

        // Relaunch: the survivor was claimed by the previous launch, so it is
        // re-claimed once (crash recovery) and then guarded again.
        let db = Database::open(&path).expect("reopen");
        let survivor = db.pending_operations("acc1").unwrap()[0].clone();
        assert_eq!(
            (survivor.status.as_str(), survivor.attempts),
            ("running", 2)
        );
        assert!(db.begin_operation_attempt_from(op_id, "running").unwrap());
        assert!(!db.begin_operation_attempt(op_id).unwrap());
        assert_eq!(db.pending_operations("acc1").unwrap()[0].attempts, 3);

        // Unknown ids: the plain claim errors, the CAS refuses.
        assert!(db.begin_operation_attempt(999_999).is_err());
        assert!(!db.begin_operation_attempt_from(999_999, "queued").unwrap());
    }

    #[test]
    fn manual_cancel_is_terminal_reclaims_blob_and_refuses_running() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "a", 1000)).unwrap();
        db.mark_read("acc1:INBOX:1", true).unwrap();
        let op_id = db.pending_operations("acc1").unwrap()[0].id;
        db.lock_conn()
            .execute(
                "INSERT INTO operation_blobs (operation_id, bytes) VALUES (?1, ?2)",
                params![op_id, b"payload-bytes".as_slice()],
            )
            .unwrap();

        // A running op is the executor's — manual cancel must refuse.
        db.set_operation_status(op_id, "running", None).unwrap();
        assert!(!db.cancel_operation(op_id).unwrap());
        assert!(db.operation_blob(op_id).unwrap().is_some());

        // failed → cancel succeeds, becomes terminal, blob reclaimed,
        // and it disappears from the outbox listing.
        db.set_operation_status(op_id, "failed", Some("boom"))
            .unwrap();
        assert!(db.cancel_operation(op_id).unwrap());
        assert!(db.operation_blob(op_id).unwrap().is_none());
        assert!(db.pending_operations("acc1").unwrap().is_empty());

        // Terminal is immutable: neither a second cancel nor a retry works.
        assert!(!db.cancel_operation(op_id).unwrap());
        assert!(!db.retry_operation(op_id).unwrap());
        assert!(db.cancel_operation(999_999).is_err());
    }

    #[test]
    fn uidvalidity_purge_cancels_only_that_mailboxes_ops() {
        let db = fresh_db();
        seed_account_mailbox(&db); // acc1:INBOX
                                   // A prefix-adjacent mailbox: "acc1:INBOX" must never match into it.
        db.upsert_mailbox(&Mailbox {
            id: "acc1:INBOX2".into(),
            account_id: "acc1".into(),
            path: "INBOX2".into(),
            name: "INBOX2".into(),
            role: None,
            delimiter: Some("/".into()),
            subscribed: false,
            selectable: true,
            sync_by_default: false,
        })
        .unwrap();
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Trash".into(),
            account_id: "acc1".into(),
            path: "Trash".into(),
            name: "Trash".into(),
            role: Some("trash".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.upsert_message(&msg(1, "a", 1000)).unwrap();
        db.upsert_message(&msg(2, "b", 1000)).unwrap();
        db.upsert_message(&msg(3, "c", 1000)).unwrap();
        let mut other = msg(1, "other", 1000);
        other.id = "acc1:INBOX2:1".into();
        other.mailbox_id = "acc1:INBOX2".into();
        other.rfc822_message_id = Some("<o1@x>".into());
        db.upsert_message(&other).unwrap();

        // Ops targeting INBOX (must be cancelled by the purge):
        db.mark_read("acc1:INBOX:1", true).unwrap(); // mark_read
        db.set_flagged("acc1:INBOX:2", true).unwrap(); // flag
        db.mark_all_read("acc1", "acc1:INBOX").unwrap(); // mark_all_read
        db.delete_message("acc1:INBOX:3").unwrap(); // move, src_path INBOX
                                                    // Ops that must SURVIVE the INBOX purge:
        db.mark_read("acc1:INBOX2:1", true).unwrap(); // other mailbox
                                                      // empty_trash rewrites the still-pending move above into a
                                                      // delete_permanent on its SOURCE (INBOX) — PR #165 review point 1.
                                                      // Its payload names "path": "INBOX" (not "src_path"), so it needs
                                                      // its OWN arm in cancel_operations_for_purged_mailbox (review round
                                                      // 2 point 2) to still be caught by this purge.
        db.empty_trash("acc1").unwrap(); // empty_trash targets Trash
        let draft = db
            .save_draft(&NewDraft {
                id: None,
                account_id: "acc1".into(),
                to: vec![DraftAddress {
                    name: None,
                    email: "b@x".into(),
                }],
                cc: vec![],
                bcc: vec![],
                subject: "s".into(),
                body_text: "t".into(),
                body_html: None,
                from_email: None,
                from_name: None,
                in_reply_to: None,
                references_header: None,
            })
            .unwrap();
        db.send_draft(draft).unwrap(); // send op: NEVER cancelled
        assert_eq!(db.pending_operations("acc1").unwrap().len(), 7);

        let cancelled = db
            .cancel_operations_for_purged_mailbox(
                "acc1",
                "acc1:INBOX",
                "INBOX",
                "cancelled: UIDVALIDITY reset purged mailbox INBOX",
            )
            .unwrap();
        assert_eq!(
            cancelled, 4,
            "mark_read + flag + mark_all_read + delete_permanent (its own dedicated arm)"
        );

        let remaining = db.pending_operations("acc1").unwrap();
        let kinds: Vec<&str> = remaining.iter().map(|o| o.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["mark_read", "empty_trash", "send"],
            "other-mailbox, Trash and send ops untouched"
        );
        assert!(remaining
            .iter()
            .all(|o| o.status == "queued" && o.attempts == 0));

        // Cancelled ops are terminal with an audit reason, not deleted.
        {
            let conn = db.lock_conn();
            let (n, reason): (i64, String) = conn
                .query_row(
                    "SELECT COUNT(*), MAX(last_error) FROM operations
                     WHERE status = 'cancelled'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(n, 4);
            assert!(reason.contains("UIDVALIDITY"), "reason recorded: {reason}");
        }

        // A Trash purge cancels the empty_trash op (its expunge would hit
        // server messages the user never saw) but not the send op.
        let cancelled_trash = db
            .cancel_operations_for_purged_mailbox("acc1", "acc1:Trash", "Trash", "cancelled: test")
            .unwrap();
        assert_eq!(cancelled_trash, 1);

        // Succeeded ops are never re-cancelled.
        let ops = db.pending_operations("acc1").unwrap();
        let inbox2_op = ops.iter().find(|o| o.kind == "mark_read").unwrap();
        db.set_operation_status(inbox2_op.id, "succeeded", None)
            .unwrap();
        assert_eq!(
            db.cancel_operations_for_purged_mailbox(
                "acc1",
                "acc1:INBOX2",
                "INBOX2",
                "cancelled: test"
            )
            .unwrap(),
            0
        );
    }

    /// Seeds `acc1:Old` (non-role, unticked) with one message
    /// (`acc1:Old:1`, provider_uid 1, rfc822 `<old1@x>`) alongside the
    /// standard `acc1:INBOX` (kept) with `acc1:INBOX:1`. Shared by every
    /// `apply_mailbox_selection` test below that doesn't need a role
    /// mailbox or a second removed mailbox.
    fn seed_kept_inbox_and_removed_old(db: &Database) {
        seed_account_mailbox(db); // acc1:INBOX, role inbox, kept
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Old".into(),
            account_id: "acc1".into(),
            path: "Old".into(),
            name: "Old".into(),
            role: None,
            delimiter: Some("/".into()),
            subscribed: false,
            selectable: true,
            sync_by_default: false,
        })
        .unwrap();
        db.upsert_message(&msg(1, "kept", 1000)).unwrap(); // acc1:INBOX:1
        let mut old_msg = msg(1, "old mail", 1000);
        old_msg.id = "acc1:Old:1".into();
        old_msg.mailbox_id = "acc1:Old".into();
        old_msg.rfc822_message_id = Some("<old1@x>".into());
        db.upsert_message(&old_msg).unwrap();
    }

    #[test]
    fn apply_mailbox_selection_removes_unselected_mailboxes_and_cascades_messages() {
        let db = fresh_db();
        seed_kept_inbox_and_removed_old(&db);

        let (removed, orphaned) = db
            .apply_mailbox_selection("acc1", &["INBOX".to_string()], false)
            .unwrap();
        assert_eq!(removed, 1);
        assert!(orphaned.is_empty(), "no attachments seeded in this test");

        // The removed mailbox and its message are gone (cascade); the kept
        // mailbox and its message survive untouched.
        assert_eq!(db.list_mailboxes("acc1").unwrap().len(), 1);
        assert_eq!(db.list_mailboxes("acc1").unwrap()[0].path, "INBOX");
        assert_eq!(db.count_messages("acc1", "acc1:INBOX").unwrap(), 1);
        assert_eq!(db.count_messages("acc1", "acc1:Old").unwrap(), 0);

        // Idempotent: applying the same selection again removes nothing more.
        assert_eq!(
            db.apply_mailbox_selection("acc1", &["INBOX".to_string()], false)
                .unwrap()
                .0,
            0
        );
    }

    #[test]
    fn apply_mailbox_selection_reports_attachment_hashes_no_other_mailbox_references() {
        let db = fresh_db();
        seed_account_mailbox(&db); // acc1:INBOX, kept
        for path in ["Old", "Old2"] {
            db.upsert_mailbox(&Mailbox {
                id: format!("acc1:{path}"),
                account_id: "acc1".into(),
                path: path.into(),
                name: path.into(),
                role: None,
                delimiter: Some("/".into()),
                subscribed: false,
                selectable: true,
                sync_by_default: false,
            })
            .unwrap();
        }
        db.upsert_message(&msg(1, "kept", 1000)).unwrap(); // acc1:INBOX:1
        let mut old1 = msg(1, "old1", 1000);
        old1.id = "acc1:Old:1".into();
        old1.mailbox_id = "acc1:Old".into();
        old1.rfc822_message_id = Some("<old1@x>".into());
        db.upsert_message(&old1).unwrap();
        let mut old2 = msg(1, "old2", 1000);
        old2.id = "acc1:Old2:1".into();
        old2.mailbox_id = "acc1:Old2".into();
        old2.rfc822_message_id = Some("<old2@x>".into());
        db.upsert_message(&old2).unwrap();

        // "shared" is cached against BOTH removed mailboxes (e.g. a forwarded
        // attachment) — only orphaned once both are gone. "kept-hash" is
        // cached against the surviving INBOX message too, so it must never
        // be reported.
        db.upsert_attachment_cache_entry("acc1:Old:1", 0, "shared", 5, 1)
            .unwrap();
        db.upsert_attachment_cache_entry("acc1:Old2:1", 0, "shared", 5, 1)
            .unwrap();
        db.upsert_attachment_cache_entry("acc1:Old:1", 1, "kept-hash", 5, 1)
            .unwrap();
        db.upsert_attachment_cache_entry("acc1:INBOX:1", 0, "kept-hash", 5, 1)
            .unwrap();

        let (removed, mut orphaned) = db
            .apply_mailbox_selection("acc1", &["INBOX".to_string()], false)
            .unwrap();
        assert_eq!(removed, 2);
        orphaned.sort();
        assert_eq!(
            orphaned,
            vec!["shared".to_string()],
            "shared is orphaned once both referencing mailboxes are gone; kept-hash survives via INBOX"
        );
    }

    #[test]
    fn apply_mailbox_selection_on_an_unknown_account_removes_nothing() {
        let db = fresh_db();
        assert_eq!(
            db.apply_mailbox_selection("ghost", &["INBOX".to_string()], false)
                .unwrap()
                .0,
            0
        );
    }

    #[test]
    fn apply_mailbox_selection_matches_paths_case_insensitively() {
        let db = fresh_db();
        seed_account_mailbox(&db); // path "INBOX"
        assert_eq!(
            db.apply_mailbox_selection("acc1", &["inbox".to_string()], false)
                .unwrap()
                .0,
            0,
            "a case-different keep path must still match the stored path"
        );
        assert_eq!(db.list_mailboxes("acc1").unwrap().len(), 1);
    }

    #[test]
    fn apply_mailbox_selection_rejects_an_empty_keep_list_even_when_forced() {
        let db = fresh_db();
        seed_kept_inbox_and_removed_old(&db);
        for force in [false, true] {
            let err = db.apply_mailbox_selection("acc1", &[], force).unwrap_err();
            assert!(err.0.contains("keep_paths must not be empty"), "{}", err.0);
        }
        // Nothing was touched.
        assert_eq!(db.list_mailboxes("acc1").unwrap().len(), 2);
    }

    // --- follow-up review: never silently drop user intent ---

    #[test]
    fn apply_mailbox_selection_blocks_on_a_pending_move_from_the_removed_folder() {
        let db = fresh_db();
        seed_kept_inbox_and_removed_old(&db);
        db.move_message_to_mailbox("acc1:Old:1", "acc1:INBOX")
            .unwrap(); // Old -> INBOX

        let err = db
            .apply_mailbox_selection("acc1", &["INBOX".to_string()], false)
            .unwrap_err();
        assert!(
            err.0.starts_with(MAILBOX_REMOVAL_BLOCKED_MSG_PREFIX),
            "{}",
            err.0
        );
        // Nothing removed: the mailbox row and the queued op both survive.
        assert_eq!(db.list_mailboxes("acc1").unwrap().len(), 2);
        assert_eq!(db.pending_operations("acc1").unwrap().len(), 1);

        let blockers = db
            .apply_mailbox_selection_blockers("acc1", &["INBOX".to_string()])
            .unwrap();
        assert_eq!(
            blockers,
            vec![MailboxSelectionBlocker {
                path: "Old".into(),
                pending_operations: 1
            }]
        );
    }

    #[test]
    fn apply_mailbox_selection_neither_blocks_nor_cancels_a_pending_move_into_the_removed_folder() {
        // Second review pass: counting/cancelling a move by its DESTINATION
        // made a pending Delete (a move INTO Trash) block every later folder
        // edit for as long as Trash stayed unticked (its role row is never
        // deleted, so it's in `to_remove` on every call) — and "Discard and
        // Remove" would have cancelled an INBOX delete under the unrelated
        // label "the user removed Trash". A move into a folder that's no
        // longer synced but still exists on the server is valid and must
        // still replay, so it must neither block the removal nor be
        // cancelled by `force`.
        let db = fresh_db();
        seed_kept_inbox_and_removed_old(&db);
        // A message in the KEPT mailbox is mid-move INTO the folder about to
        // be removed.
        db.move_message_to_mailbox("acc1:INBOX:1", "acc1:Old")
            .unwrap();

        let (removed, _) = db
            .apply_mailbox_selection("acc1", &["INBOX".to_string()], false)
            .expect("a move whose DESTINATION is the removed folder must never block");
        assert_eq!(
            removed, 1,
            "Old is removed despite the pending move into it"
        );
        assert_eq!(
            db.list_mailboxes("acc1").unwrap().len(),
            1,
            "Old's row is gone (no role)"
        );

        let ops = db.pending_operations("acc1").unwrap();
        assert_eq!(ops.len(), 1, "the move op survives untouched");
        assert_eq!(ops[0].kind, "move");
        assert_eq!(
            ops[0].status, "queued",
            "never cancelled — this is still a live, replayable action"
        );
    }

    #[test]
    fn apply_mailbox_selection_a_pending_delete_never_blocks_removing_an_unticked_role_folder() {
        // The exact regression named by the second review: Trash (role,
        // unticked) stays in `to_remove` on EVERY call — unticking it never
        // deletes its row, only re-adding it to `keep_paths` would. A
        // pending Delete from INBOX (a move whose destination is Trash)
        // must never block that repeated, harmless "removal", and `force`
        // must never cancel the delete under the unrelated label "the user
        // removed Trash".
        let db = fresh_db();
        seed_account_mailbox(&db); // acc1:INBOX, kept
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Trash".into(),
            account_id: "acc1".into(),
            path: "Trash".into(),
            name: "Trash".into(),
            role: Some("trash".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.upsert_message(&msg(1, "kept", 1000)).unwrap(); // acc1:INBOX:1
        db.delete_message("acc1:INBOX:1").unwrap(); // move: src=INBOX, dest=Trash

        // Unforced: succeeds even though Trash is always in to_remove and a
        // pending move's DESTINATION is Trash.
        let (removed, _) = db
            .apply_mailbox_selection("acc1", &["INBOX".to_string()], false)
            .expect("a pending delete must never block an unrelated folder edit");
        assert_eq!(removed, 1);
        let after_unforced = db.pending_operations("acc1").unwrap();
        assert_eq!(after_unforced.len(), 1, "the delete survives");
        assert_eq!(after_unforced[0].status, "queued");

        // Forced (as "Discard and Remove" would call): still never cancels
        // it — nothing else blocks Trash.
        let (removed2, _) = db
            .apply_mailbox_selection("acc1", &["INBOX".to_string()], true)
            .unwrap();
        assert_eq!(
            removed2, 1,
            "idempotent — Trash is 'removed' again on every call"
        );
        let after_forced = db.pending_operations("acc1").unwrap();
        assert_eq!(
            after_forced.len(),
            1,
            "force never cancels the delete under Trash's name"
        );
        assert_eq!(after_forced[0].status, "queued");
    }

    #[test]
    fn apply_mailbox_selection_blocks_on_a_pending_flag_and_on_empty_trash() {
        let db = fresh_db();
        seed_kept_inbox_and_removed_old(&db);
        db.set_flagged("acc1:Old:1", true).unwrap();

        let err = db
            .apply_mailbox_selection("acc1", &["INBOX".to_string()], false)
            .unwrap_err();
        assert!(
            err.0.starts_with(MAILBOX_REMOVAL_BLOCKED_MSG_PREFIX),
            "{}",
            err.0
        );

        // empty_trash blocks a Trash removal the same way.
        db.cancel_operation(db.pending_operations("acc1").unwrap()[0].id)
            .unwrap();
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Trash".into(),
            account_id: "acc1".into(),
            path: "Trash".into(),
            name: "Trash".into(),
            role: Some("trash".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.empty_trash("acc1").unwrap();
        let err = db
            .apply_mailbox_selection("acc1", &["INBOX".to_string(), "Old".to_string()], false)
            .unwrap_err();
        assert!(
            err.0.starts_with(MAILBOX_REMOVAL_BLOCKED_MSG_PREFIX),
            "{}",
            err.0
        );
    }

    /// Merge with main: Empty Trash (PR #165) rewrites a pending Delete (a
    /// move out of "Old" into Trash) into a `delete_permanent` whose `path`
    /// is "Old". That op still touches "Old", so removing "Old" must block
    /// on it exactly as `cancel_operations_for_purged_mailbox` would cancel
    /// it -- the precheck and the forced cancellation match the same ops.
    #[test]
    fn apply_mailbox_selection_blocks_on_a_pending_delete_permanent() {
        let db = fresh_db();
        seed_kept_inbox_and_removed_old(&db);
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Trash".into(),
            account_id: "acc1".into(),
            path: "Trash".into(),
            name: "Trash".into(),
            role: Some("trash".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.delete_message("acc1:Old:1").unwrap(); // a move Old -> Trash
        db.empty_trash("acc1").unwrap(); // rewrites it to delete_permanent
        let ops = db.pending_operations("acc1").unwrap();
        assert!(
            ops.iter().any(|o| o.kind == "delete_permanent"),
            "sanity: the move became a delete_permanent: {ops:?}"
        );

        let keep = ["INBOX".to_string(), "Trash".to_string()];
        let err = db
            .apply_mailbox_selection("acc1", &keep, false)
            .unwrap_err();
        assert!(
            err.0.starts_with(MAILBOX_REMOVAL_BLOCKED_MSG_PREFIX),
            "{}",
            err.0
        );
        let blockers = db.apply_mailbox_selection_blockers("acc1", &keep).unwrap();
        assert_eq!(blockers.len(), 1, "{blockers:?}");
        assert_eq!(blockers[0].path, "Old");
        assert_eq!(blockers[0].pending_operations, 1);

        db.apply_mailbox_selection("acc1", &keep, true).unwrap();
        let status: String = db
            .lock_conn()
            .query_row(
                "SELECT status FROM operations WHERE kind = 'delete_permanent'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "cancelled", "forced removal cancels it");
    }

    #[test]
    fn apply_mailbox_selection_force_cancels_the_blocking_ops_with_a_truthful_reason() {
        let db = fresh_db();
        seed_kept_inbox_and_removed_old(&db);
        db.mark_read("acc1:INBOX:1", true).unwrap(); // survives (kept mailbox)
        db.mark_read("acc1:Old:1", true).unwrap(); // blocks, then cancelled with force
        let draft = db
            .save_draft(&NewDraft {
                id: None,
                account_id: "acc1".into(),
                to: vec![DraftAddress {
                    name: None,
                    email: "b@x".into(),
                }],
                cc: vec![],
                bcc: vec![],
                subject: "s".into(),
                body_text: "t".into(),
                body_html: None,
                from_email: None,
                from_name: None,
                in_reply_to: None,
                references_header: None,
            })
            .unwrap();
        db.send_draft(draft).unwrap(); // send op: mailbox-independent, never blocks/cancels

        // Unforced: refused, nothing touched.
        assert!(db
            .apply_mailbox_selection("acc1", &["INBOX".to_string()], false)
            .is_err());
        assert_eq!(db.list_mailboxes("acc1").unwrap().len(), 2);

        // Forced: the folder is removed and its op cancelled with a
        // TRUTHFUL, folder-edit-specific reason — never the UIDVALIDITY text.
        let (removed, _) = db
            .apply_mailbox_selection("acc1", &["INBOX".to_string()], true)
            .unwrap();
        assert_eq!(removed, 1);
        assert_eq!(db.list_mailboxes("acc1").unwrap().len(), 1);

        let remaining = db.pending_operations("acc1").unwrap();
        let kinds: Vec<&str> = remaining.iter().map(|o| o.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["mark_read", "send"],
            "the kept mailbox's op and the mailbox-independent send survive"
        );
        let (n, reason): (i64, String) = db
            .lock_conn()
            .query_row(
                "SELECT COUNT(*), MAX(last_error) FROM operations WHERE status = 'cancelled'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(n, 1, "only the removed mailbox's op is cancelled");
        assert!(
            reason.contains("user removed folder 'Old' from sync"),
            "truthful, specific reason: {reason}"
        );
        assert!(
            !reason.contains("UIDVALIDITY"),
            "never the reset's reason: {reason}"
        );
    }

    // --- follow-up review: special-use folders survive -------

    /// Bug hunt 2026-10-12 (#3456): a folder gone from the server is removed
    /// outright — row, mail, cursor — even when it has a role, and the ops
    /// that touched it are cancelled with a reason that says why.
    #[test]
    fn remove_mailboxes_gone_from_server_deletes_the_row_and_cancels_its_ops() {
        let db = fresh_db();
        seed_account_mailbox(&db); // acc1:INBOX
        db.set_account_write_enabled("acc1", true).unwrap();
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Deleted Items".into(),
            account_id: "acc1".into(),
            path: "Deleted Items".into(),
            name: "Deleted Items".into(),
            role: Some("trash".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.upsert_message(&msg(1, "kept", 1000)).unwrap(); // acc1:INBOX:1
        let mut gone = msg(1, "gone", 1000);
        gone.id = "acc1:Deleted Items:1".into();
        gone.mailbox_id = "acc1:Deleted Items".into();
        gone.rfc822_message_id = Some("<t1@x>".into());
        db.upsert_message(&gone).unwrap();
        db.set_sync_cursor(
            "acc1",
            "acc1:Deleted Items",
            &SyncCursor {
                uid_validity: Some(9),
                uid_next: Some(2),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
        db.mark_read("acc1:Deleted Items:1", true).unwrap();
        db.mark_read("acc1:INBOX:1", true).unwrap();
        assert_eq!(db.pending_operations("acc1").unwrap().len(), 2);

        let removed = db
            .remove_mailboxes_gone_from_server(
                "acc1",
                &["Deleted Items".to_string(), "Never Stored".to_string()],
            )
            .unwrap();
        assert_eq!(removed.paths, vec!["Deleted Items".to_string()]);
        assert_eq!(removed.messages, 1);
        assert_eq!(removed.cancelled_ops, 1);

        let mailboxes = db.list_mailboxes("acc1").unwrap();
        assert_eq!(mailboxes.len(), 1, "the role row goes too");
        assert_eq!(mailboxes[0].path, "INBOX");
        assert_eq!(db.count_messages("acc1", "acc1:INBOX").unwrap(), 1);
        assert!(db
            .get_message_detail("acc1:Deleted Items:1")
            .unwrap()
            .is_none());
        assert!(db
            .get_sync_cursor("acc1", "acc1:Deleted Items")
            .unwrap()
            .is_none());
        // The other folder's op is untouched; the cancelled one says why.
        let pending = db.pending_operations("acc1").unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].payload_json.contains("acc1:INBOX:1"));
        let conn = db.lock_conn();
        let reason: String = conn
            .query_row(
                "SELECT last_error FROM operations WHERE status = 'cancelled'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            reason,
            "cancelled: folder 'Deleted Items' no longer exists on the server"
        );
    }

    /// Paths are matched exactly as stored: IMAP names are case-sensitive.
    #[test]
    fn remove_mailboxes_gone_from_server_matches_paths_exactly() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "kept", 1000)).unwrap();
        let removed = db
            .remove_mailboxes_gone_from_server("acc1", &["inbox".to_string()])
            .unwrap();
        assert!(removed.paths.is_empty());
        assert_eq!(db.count_messages("acc1", "acc1:INBOX").unwrap(), 1);
    }

    #[test]
    fn apply_mailbox_selection_keeps_role_mailboxes_and_purges_only_their_mail() {
        let db = fresh_db();
        seed_account_mailbox(&db); // acc1:INBOX, role inbox, kept
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Deleted Items".into(),
            account_id: "acc1".into(),
            path: "Deleted Items".into(),
            name: "Deleted Items".into(),
            role: Some("trash".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.upsert_message(&msg(1, "kept", 1000)).unwrap(); // acc1:INBOX:1
        let mut trashed = msg(1, "trashed", 1000);
        trashed.id = "acc1:Deleted Items:1".into();
        trashed.mailbox_id = "acc1:Deleted Items".into();
        trashed.rfc822_message_id = Some("<t1@x>".into());
        db.upsert_message(&trashed).unwrap();
        db.set_sync_cursor(
            "acc1",
            "acc1:Deleted Items",
            &SyncCursor {
                uid_validity: Some(9),
                uid_next: Some(2),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();

        let (removed, _) = db
            .apply_mailbox_selection("acc1", &["INBOX".to_string()], false)
            .unwrap();
        assert_eq!(
            removed, 1,
            "unticking a role mailbox still counts as removed from sync"
        );

        // The row and role survive; only its mail and cursor are purged.
        let mailboxes = db.list_mailboxes("acc1").unwrap();
        assert_eq!(mailboxes.len(), 2, "the Trash row is kept, never deleted");
        let trash = mailboxes
            .iter()
            .find(|m| m.role.as_deref() == Some("trash"))
            .expect("trash role survives unticking");
        assert_eq!(
            trash.path, "Deleted Items",
            "the real server path is kept, never renamed"
        );
        assert_eq!(db.count_messages("acc1", "acc1:Deleted Items").unwrap(), 0);
        assert!(
            db.get_sync_cursor("acc1", "acc1:Deleted Items")
                .unwrap()
                .is_none(),
            "the cursor is purged too, so a later re-tick is a fresh first import"
        );

        // Delete still resolves Trash by role to the REAL path — no stray
        // default-named mailbox is created (the exact regression this fixes).
        db.delete_message("acc1:INBOX:1").unwrap();
        let ops = db.pending_operations("acc1").unwrap();
        let mv = ops
            .iter()
            .find(|o| o.kind == "move")
            .expect("delete enqueues a move");
        assert!(
            mv.payload_json.contains("\"dest_path\":\"Deleted Items\""),
            "moves to the real Trash path, not a default one: {}",
            mv.payload_json
        );
        assert_eq!(
            db.list_mailboxes("acc1").unwrap().len(),
            2,
            "no stray default-named Trash mailbox was created"
        );

        // Empty Trash still finds the (unticked) Trash mailbox.
        assert!(
            db.empty_trash("acc1").is_ok(),
            "empty_trash must still resolve the kept role row"
        );
    }

    #[test]
    fn upsert_stores_html_bodies_and_remote_flag() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let mut m = msg(1, "HTML test", 1000);
        m.html_body = Some("<p>raw <script>x</script></p>".into());
        m.sanitized_html = Some("<p>raw </p>".into());
        m.has_remote_content = true;
        db.upsert_message(&m).unwrap();

        let detail = db.get_message_detail(&m.id).unwrap().unwrap();
        assert_eq!(detail.sanitized_html.as_deref(), Some("<p>raw </p>"));
        assert!(detail.has_remote_content);

        // raw html persisted (future re-sanitize / forward-quoting source)
        let conn = db.lock_conn();
        let raw: Option<String> = conn
            .query_row(
                "SELECT html_body FROM message_bodies WHERE message_id = ?1",
                rusqlite::params![m.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(raw.as_deref(), Some("<p>raw <script>x</script></p>"));
    }

    #[test]
    fn schema_is_at_latest_version() {
        // 6: html bodies. 7: drafts. 9: contacts (8 reserved during parallel
        // development). 10: AI queue (PR #49). 11: projects (slice 8b).
        // 12: contact enrichment (slice 8d). 13: mailbox discovery metadata.
        // 14: incoming attachment descriptors. 15: operation payload blobs.
        // 16: sync diagnostics state. 17: shared evidence-backed links
        // (replaces project_emails/project_contacts). 18: work items.
        // 19: per-account write policy (fail-closed evaluation mode).
        // 20: undo-send window (S25 — operations.not_before_epoch).
        // 21: incoming attachment addressing (S32 — section/content_id/is_inline).
        // 22: content-addressed attachment_cache table (S32).
        // 23: unified-inbox supporting indexes (S34 — idx_mailboxes_role,
        // idx_messages_sent_at).
        // 24: snoozed_messages table (S43 — snooze and follow-up reminders).
        // 25: markdown compose (S46 — drafts.body_html).
        // 26: account_identities (S47 — drafts.from_email/from_name).
        // 27: List-Unsubscribe columns on messages (S44).
        // 28: smart_mailboxes (S41 — saved S39 search-grammar queries
        // with cached, capped counts).
        // 29: trust surface (S48 — message_trust,
        // sender_remote_content_policy).
        // 30: outgoing draft_attachments + draft_attachment_blobs (S33).
        // 31: message_embeddings (S52 — one vector per message, stored as a
        // little-endian f32 BLOB by the embed_message background job).
        // 32: calendar_invites (S55 — Castellum's own accept/decline/
        // tentative memory for a parsed .ics invite).
        // 33: thread_extractions (S54 — schema-validated action_item/date/
        // amount items from the extract_thread_items AI feature).
        // 34: saved_filters (S40 — declarative filters backing smart
        // mailboxes and rules).
        // 35: server-mirror columns on drafts (S36 — renumbered down from
        // S36's original claim of 36 during the 2026-09-25 batch merge, once
        // S30, the batch's other stage, turned out to need no migration of
        // its own; S37 is not in this batch and will claim its own id when
        // it lands separately).
        // 36: contact_lookup_index (S64 — expression index on
        // message_addresses(lower(trim(address))) so refresh_contact_
        // aggregates' derived-name subquery seeks instead of scanning; see
        // project-docs/perf/read-path-budgets.md). Claimed 36, not S64's
        // original 35, once the same batch merge gave 35 to S36 first.
        // 37: thread_rebuild_state (S64 review fix — per-account "threads
        // dirty" marker so sync's skip-rebuild-threads optimisation stays
        // correct when a LOCAL mutation, not a remote change, is what moved
        // a thread's unread_count/message_count).
        // 38: message_fts_rowid mapping + message_fts rebuild (the tracker
        // #2500 — rowid-keyed FTS so DELETE/UPDATE by message id is an
        // indexed lookup, not a full scan of the UNINDEXED column).
        // 39: thread_dirty_messages + thread_rebuild_state.force_full
        // ( — incremental thread rebuild tracks WHICH messages
        // moved instead of only a per-account boolean).
        // 40: sync_cursors.descriptor_checkpoint_uid + message_decode_skips
        // ( — chunked descriptor fetch checkpoint and the
        // backed-off decode-skip retry set).
        // 41: tags_and_mute (S42 — tag_definitions, message_keywords,
        // thread_mutes, threads.is_muted; written as 39 on its branch and
        // renumbered past main's 39/40 at merge time).
        // 47: saved_views (AW3; 46 is reserved by another stage).
        // 42: smart_mailbox_groups (MM-1 — smart_mailboxes.group_json and
        // scope_json for nested condition groups).
        // Pinned against latest_schema_version() so it tracks additions;
        // the == 42 assertion documents the current watermark.
        let db = fresh_db();
        assert_eq!(db.schema_version().unwrap(), crate::latest_schema_version());
        // 46: suggestions (AW2); 47: saved views (AW3); 48: notifications/VIP (S59); 49: project timeline; 50: work-item provenance; 51: messages(account_id, rfc822_message_id) index.
        assert_eq!(crate::latest_schema_version(), 57);
    }

    // --- drafts -------------------------------------------------------------

    fn draft_input(subject: &str) -> NewDraft {
        NewDraft {
            id: None,
            account_id: "acc1".into(),
            to: vec![DraftAddress {
                name: Some("Bob".into()),
                email: "bob@local.test".into(),
            }],
            cc: vec![DraftAddress {
                name: None,
                email: "carol@local.test".into(),
            }],
            bcc: vec![DraftAddress {
                name: None,
                email: "dave@local.test".into(),
            }],
            subject: subject.into(),
            body_text: "Hello there.\n".into(),
            body_html: None,
            from_email: None,
            from_name: None,
            in_reply_to: Some("<parent@local.test>".into()),
            references_header: Some("<root@local.test> <parent@local.test>".into()),
        }
    }

    fn new_draft_with_html(
        account_id: &str,
        to: &str,
        body_text: &str,
        body_html: Option<&str>,
    ) -> NewDraft {
        NewDraft {
            id: None,
            account_id: account_id.into(),
            to: vec![DraftAddress {
                name: None,
                email: to.into(),
            }],
            cc: vec![],
            bcc: vec![],
            subject: "s".into(),
            body_text: body_text.into(),
            body_html: body_html.map(str::to_owned),
            from_email: None,
            from_name: None,
            in_reply_to: None,
            references_header: None,
        }
    }

    /// `new_draft_with_html` plus an optional chosen identity override (S47).
    fn new_draft_with_identity(
        account_id: &str,
        to: &str,
        from: Option<(&str, Option<&str>)>,
    ) -> NewDraft {
        let mut d = new_draft_with_html(account_id, to, "body", None);
        if let Some((email, name)) = from {
            d.from_email = Some(email.into());
            d.from_name = name.map(str::to_owned);
        }
        d
    }

    #[test]
    fn save_draft_round_trips_body_html_when_present() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let id = db
            .save_draft(&new_draft_with_html(
                "acc1",
                "b@x",
                "**hi**",
                Some("<p><strong>hi</strong></p>"),
            ))
            .unwrap();
        let stored = db.get_draft(id).unwrap().unwrap();
        assert_eq!(
            stored.body_html.as_deref(),
            Some("<p><strong>hi</strong></p>")
        );
        assert_eq!(stored.body_text, "**hi**");
    }

    #[test]
    fn save_draft_defaults_body_html_to_none_when_markdown_is_off() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let id = db
            .save_draft(&new_draft_with_html("acc1", "b@x", "plain text", None))
            .unwrap();
        let stored = db.get_draft(id).unwrap().unwrap();
        assert_eq!(stored.body_html, None);
    }

    #[test]
    fn updating_a_draft_can_turn_markdown_off_again() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let id = db
            .save_draft(&new_draft_with_html(
                "acc1",
                "b@x",
                "**hi**",
                Some("<p><strong>hi</strong></p>"),
            ))
            .unwrap();
        let mut update = new_draft_with_html("acc1", "b@x", "hi", None);
        update.id = Some(id);
        db.save_draft(&update).unwrap();
        let stored = db.get_draft(id).unwrap().unwrap();
        assert_eq!(
            stored.body_html, None,
            "un-checking Markdown must clear the stored html"
        );
        assert_eq!(stored.body_text, "hi");
    }

    #[test]
    fn save_draft_round_trips_from_email_and_from_name() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let id = db
            .save_draft(&new_draft_with_identity(
                "acc1",
                "b@x",
                Some(("alias@x", Some("Alias"))),
            ))
            .unwrap();
        let stored = db.get_draft(id).unwrap().unwrap();
        assert_eq!(stored.from_email.as_deref(), Some("alias@x"));
        assert_eq!(stored.from_name.as_deref(), Some("Alias"));
    }

    #[test]
    fn save_draft_defaults_from_email_to_none_when_no_identity_was_chosen() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let id = db
            .save_draft(&new_draft_with_identity("acc1", "b@x", None))
            .unwrap();
        let stored = db.get_draft(id).unwrap().unwrap();
        assert_eq!(stored.from_email, None);
        assert_eq!(stored.from_name, None);
    }

    #[test]
    fn new_draft_has_no_server_mirror_by_default() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let id = db.save_draft(&draft_input("First")).unwrap();
        let draft = db.get_draft(id).unwrap().unwrap();
        assert!(draft.server_mailbox_path.is_none());
        assert!(draft.server_uid.is_none());
        assert!(draft.server_uid_validity.is_none());
        assert!(draft.server_message_id.is_none());
        assert!(draft.server_saved_at_epoch.is_none());
    }

    fn seed_drafts_role_mailbox(db: &Database, account_id: &str, path: &str) {
        db.upsert_mailbox(&Mailbox {
            id: format!("{account_id}:{path}"),
            account_id: account_id.to_string(),
            path: path.to_string(),
            name: path.to_string(),
            role: Some("drafts".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
    }

    fn empty_draft(subject: &str) -> NewDraft {
        NewDraft {
            id: None,
            account_id: "acc1".into(),
            to: vec![],
            cc: vec![],
            bcc: vec![],
            subject: subject.into(),
            body_text: "".into(),
            body_html: None,
            from_email: None,
            from_name: None,
            in_reply_to: None,
            references_header: None,
        }
    }

    #[test]
    fn queue_server_draft_save_creates_drafts_mailbox_and_op_with_blob() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.set_account_write_enabled("acc1", true).unwrap();
        let id = db.save_draft(&empty_draft("Hi")).unwrap();

        db.queue_server_draft_save(id, "<msg1@x>", b"raw bytes")
            .unwrap();

        let mailboxes = db.list_mailboxes("acc1").unwrap();
        assert!(mailboxes
            .iter()
            .any(|m| m.role.as_deref() == Some("drafts") && m.path == "Drafts"));
        let ops = db.pending_operations("acc1").unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].kind, "server_draft_save");
        let payload: serde_json::Value = serde_json::from_str(&ops[0].payload_json).unwrap();
        assert_eq!(payload["draft_id"], id);
        assert_eq!(payload["dest_path"], "Drafts");
        assert_eq!(db.operation_blob(ops[0].id).unwrap().unwrap(), b"raw bytes");
    }

    #[test]
    fn queue_server_draft_save_is_a_noop_once_the_draft_left_draft_status() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.set_account_write_enabled("acc1", true).unwrap();
        let mut d = empty_draft("Hi");
        d.to = vec![DraftAddress {
            name: None,
            email: "b@x".into(),
        }];
        let id = db.save_draft(&d).unwrap();
        db.send_draft(id).unwrap();

        db.queue_server_draft_save(id, "<msg1@x>", b"raw bytes")
            .unwrap();
        let ops: Vec<_> = db
            .pending_operations("acc1")
            .unwrap()
            .into_iter()
            .filter(|o| o.kind == "server_draft_save")
            .collect();
        assert!(
            ops.is_empty(),
            "a debounced save that lands after Send must be a silent no-op"
        );
    }

    #[test]
    fn record_server_draft_saved_queues_cleanup_only_when_superseding_a_different_uid() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let id = db.save_draft(&empty_draft("Hi")).unwrap();

        // First save: nothing prior to clean up.
        let ok = db
            .record_server_draft_saved(id, "acc1", "Drafts", Some(1), Some(10), "<m1@x>")
            .unwrap();
        assert!(ok);
        assert!(db.pending_operations("acc1").unwrap().is_empty());

        // Second save (edit): supersedes uid 10 -> queues its cleanup.
        let ok = db
            .record_server_draft_saved(id, "acc1", "Drafts", Some(1), Some(11), "<m2@x>")
            .unwrap();
        assert!(ok);
        let ops = db.pending_operations("acc1").unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].kind, "server_draft_cleanup");
        let payload: serde_json::Value = serde_json::from_str(&ops[0].payload_json).unwrap();
        assert_eq!(payload["uid"], 10);

        let draft = db.get_draft(id).unwrap().unwrap();
        assert_eq!(draft.server_uid, Some(11));
        assert_eq!(draft.server_message_id.as_deref(), Some("<m2@x>"));
    }

    /// FP06: Cancel on a failed send returns the message to Drafts instead
    /// of leaving it locked in the outbox.
    #[test]
    fn manual_cancel_of_failed_send_returns_draft_to_drafts() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.set_account_write_enabled("acc1", true).unwrap();
        let mut d = empty_draft("Hi");
        d.to = vec![DraftAddress {
            name: None,
            email: "b@x".into(),
        }];
        let id = db.save_draft(&d).unwrap();
        db.send_draft(id).unwrap();
        let op = db
            .pending_operations("acc1")
            .unwrap()
            .into_iter()
            .find(|o| o.kind == "send")
            .expect("send op queued");
        db.set_operation_status(op.id, "failed", Some("smtp refused"))
            .unwrap();
        assert!(db.cancel_operation(op.id).unwrap());
        let draft = db.get_draft(id).unwrap().unwrap();
        assert_eq!(draft.status, "draft");
        let mut edited = empty_draft("Hi again");
        edited.id = Some(id);
        db.save_draft(&edited).expect("cancelled draft is editable");
        db.delete_draft(id).expect("cancelled draft is deletable");
    }

    /// FP06: moving a draft to another From account drops the old account's
    /// server mirror (one cleanup on the OLD account) and never carries its
    /// UID into the new account.
    #[test]
    fn save_draft_account_change_cleans_old_mirror_and_clears_columns() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_account(&Account {
            id: "acc2".into(),
            email: "two@local.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        let id = db.save_draft(&empty_draft("Hi")).unwrap();
        db.record_server_draft_saved(id, "acc1", "Drafts", Some(1), Some(10), "<m1@x>")
            .unwrap();
        let mut moved = empty_draft("Hi");
        moved.id = Some(id);
        moved.account_id = "acc2".into();
        db.save_draft(&moved).unwrap();

        let ops = db.pending_operations("acc1").unwrap();
        let cleanups: Vec<_> = ops
            .iter()
            .filter(|o| o.kind == "server_draft_cleanup")
            .collect();
        assert_eq!(cleanups.len(), 1, "{ops:?}");
        let payload: serde_json::Value = serde_json::from_str(&cleanups[0].payload_json).unwrap();
        assert_eq!(payload["uid"], 10);
        assert!(db
            .pending_operations("acc2")
            .unwrap()
            .iter()
            .all(|o| o.kind != "server_draft_cleanup"));
        let draft = db.get_draft(id).unwrap().unwrap();
        assert_eq!(draft.account_id, "acc2");
        assert!(draft.server_uid.is_none() && draft.server_mailbox_path.is_none());
        assert!(draft.server_uid_validity.is_none() && draft.server_message_id.is_none());
    }

    /// S36 review fix: reproduces the reviewer's repro at the DB layer
    /// (queue a save, then send/discard *before* replay runs, then replay).
    /// `send_draft` flips status to 'outbox' while the mirror is still
    /// unresolved (the save's op is still queued); this simulates the
    /// residual race where replay's own `draft_status` guard passed before
    /// the APPEND, but the row changed under it during the round-trip — the
    /// just-appended copy must still be queued for cleanup, not recorded as
    /// the draft's live mirror.
    #[test]
    fn record_server_draft_saved_queues_cleanup_when_draft_left_draft_status() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.set_account_write_enabled("acc1", true).unwrap();
        let mut d = empty_draft("Hi");
        d.to = vec![DraftAddress {
            name: None,
            email: "b@x".into(),
        }];
        let id = db.save_draft(&d).unwrap();
        db.send_draft(id).unwrap();

        let recorded = db
            .record_server_draft_saved(id, "acc1", "Drafts", Some(1), Some(10), "<m1@x>")
            .unwrap();
        assert!(!recorded, "must not record a mirror on a non-draft row");

        let draft = db.get_draft(id).unwrap().unwrap();
        assert!(
            draft.server_uid.is_none(),
            "a non-draft row's mirror fields stay untouched: {draft:?}"
        );

        let ops = db.pending_operations("acc1").unwrap();
        let cleanup = ops
            .iter()
            .find(|o| o.kind == "server_draft_cleanup")
            .expect("the just-appended copy must be queued for cleanup");
        let payload: serde_json::Value = serde_json::from_str(&cleanup.payload_json).unwrap();
        assert_eq!(payload["uid"], 10);
        assert_eq!(payload["mailbox_path"], "Drafts");
    }

    #[test]
    fn draft_status_reflects_the_row_and_none_once_discarded() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let id = db.save_draft(&empty_draft("Hi")).unwrap();
        assert_eq!(db.draft_status(id).unwrap(), Some("draft".to_string()));
        db.delete_draft(id).unwrap();
        assert_eq!(db.draft_status(id).unwrap(), None);
    }

    #[test]
    fn send_draft_queues_cleanup_for_a_known_server_mirror() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.set_account_write_enabled("acc1", true).unwrap();
        let mut d = empty_draft("Hi");
        d.to = vec![DraftAddress {
            name: None,
            email: "b@x".into(),
        }];
        let id = db.save_draft(&d).unwrap();
        db.record_server_draft_saved(id, "acc1", "Drafts", Some(1), Some(10), "<m1@x>")
            .unwrap();

        db.send_draft(id).unwrap();

        let ops = db.pending_operations("acc1").unwrap();
        let cleanup = ops
            .iter()
            .find(|o| o.kind == "server_draft_cleanup")
            .expect("cleanup queued");
        let payload: serde_json::Value = serde_json::from_str(&cleanup.payload_json).unwrap();
        assert_eq!(payload["uid"], 10);
        assert_eq!(payload["mailbox_path"], "Drafts");
    }

    #[test]
    fn delete_draft_queues_cleanup_for_a_known_server_mirror() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let id = db.save_draft(&empty_draft("Hi")).unwrap();
        db.record_server_draft_saved(id, "acc1", "Drafts", Some(1), Some(10), "<m1@x>")
            .unwrap();

        db.delete_draft(id).unwrap();

        let ops = db.pending_operations("acc1").unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].kind, "server_draft_cleanup");
        assert!(db.get_draft(id).unwrap().is_none());
    }

    fn seed_second_account(db: &Database) {
        db.upsert_account(&Account {
            id: "acc2".into(),
            email: "two@local.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.set_account_write_enabled("acc2", true).unwrap();
    }

    fn ops_of_kind(db: &Database, account_id: &str, kind: &str) -> Vec<serde_json::Value> {
        db.pending_operations(account_id)
            .unwrap()
            .iter()
            .filter(|o| o.kind == kind)
            .map(|o| serde_json::from_str(&o.payload_json).unwrap())
            .collect()
    }

    ///: a save the old account queued but has not delivered
    /// (offline, backoff) is cancelled with the account change, bytes and all.
    #[test]
    fn save_draft_account_change_cancels_the_old_accounts_pending_saves() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_second_account(&db);
        let id = db.save_draft(&empty_draft("Hi")).unwrap();
        let other = db.save_draft(&empty_draft("Untouched")).unwrap();
        db.queue_server_draft_save(id, "<m1@x>", b"stale bytes")
            .unwrap();
        db.queue_server_draft_save(other, "<o1@x>", b"other bytes")
            .unwrap();
        let op_id = db.pending_operations("acc1").unwrap()[0].id;

        let mut moved = empty_draft("Hi");
        moved.id = Some(id);
        moved.account_id = "acc2".into();
        db.save_draft(&moved).unwrap();

        let saves = ops_of_kind(&db, "acc1", "server_draft_save");
        assert_eq!(saves.len(), 1, "only the moved draft's save is cancelled");
        assert_eq!(saves[0]["draft_id"], other);
        assert!(
            db.operation_blob(op_id).unwrap().is_none(),
            "the cancelled save's bytes are dropped"
        );
        assert!(db.pending_operations("acc2").unwrap().is_empty());
    }

    ///, in-flight variant: the From switch raced the APPEND
    /// round-trip, so the copy already sits in the OLD account's Drafts.
    #[test]
    fn record_server_draft_saved_never_puts_one_accounts_uid_on_anothers_draft() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_second_account(&db);
        let id = db.save_draft(&empty_draft("Hi")).unwrap();
        let mut moved = empty_draft("Hi");
        moved.id = Some(id);
        moved.account_id = "acc2".into();
        db.save_draft(&moved).unwrap();

        // acc1's replay finishes its APPEND (uid 10 in acc1's Drafts).
        let recorded = db
            .record_server_draft_saved(id, "acc1", "Drafts", Some(4), Some(10), "<m1@x>")
            .unwrap();
        assert!(!recorded);

        let draft = db.get_draft(id).unwrap().unwrap();
        assert_eq!(draft.account_id, "acc2");
        assert!(
            draft.server_uid.is_none() && draft.server_mailbox_path.is_none(),
            "acc2's draft must not point at a UID in acc1's Drafts"
        );
        let cleanups = ops_of_kind(&db, "acc1", "server_draft_cleanup");
        assert_eq!(cleanups.len(), 1, "the stray copy is removed where it is");
        assert_eq!(cleanups[0]["uid"], 10);
        assert_eq!(cleanups[0]["uid_validity"], 4);
        assert!(db.pending_operations("acc2").unwrap().is_empty());

        // Regression: acc2's next save must not treat acc1's copy as its own
        // superseded mirror and delete uid 10 in acc2's Drafts.
        assert!(db
            .record_server_draft_saved(id, "acc2", "Drafts", Some(9), Some(3), "<m2@x>")
            .unwrap());
        assert!(ops_of_kind(&db, "acc2", "server_draft_cleanup").is_empty());
        db.delete_draft(id).unwrap();
        let acc2_cleanups = ops_of_kind(&db, "acc2", "server_draft_cleanup");
        assert_eq!(acc2_cleanups.len(), 1);
        assert_eq!(acc2_cleanups[0]["uid"], 3, "acc2 only ever deletes its own");
    }

    /// The draft was discarded during the APPEND round-trip: the copy has no
    /// row to track it, so it is removed rather than left behind for good.
    #[test]
    fn record_server_draft_saved_cleans_up_the_copy_of_a_discarded_draft() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let id = db.save_draft(&empty_draft("Hi")).unwrap();
        db.delete_draft(id).unwrap();

        assert!(!db
            .record_server_draft_saved(id, "acc1", "Drafts", Some(4), Some(10), "<m1@x>")
            .unwrap());
        let cleanups = ops_of_kind(&db, "acc1", "server_draft_cleanup");
        assert_eq!(cleanups.len(), 1);
        assert_eq!(cleanups[0]["uid"], 10);
    }

    ///: every route that queues a cleanup snapshots the UID
    /// space of the UID it deletes.
    #[test]
    fn server_draft_cleanup_carries_the_uid_validity_of_the_copy_it_deletes() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_second_account(&db);

        // Superseded by the next save.
        let a = db.save_draft(&empty_draft("A")).unwrap();
        db.record_server_draft_saved(a, "acc1", "Drafts", Some(4), Some(10), "<a1@x>")
            .unwrap();
        db.record_server_draft_saved(a, "acc1", "Drafts", Some(4), Some(11), "<a2@x>")
            .unwrap();
        // Discarded.
        db.delete_draft(a).unwrap();
        // Sent.
        let mut d = empty_draft("B");
        d.to = vec![DraftAddress {
            name: None,
            email: "b@x".into(),
        }];
        let b = db.save_draft(&d).unwrap();
        db.record_server_draft_saved(b, "acc1", "Drafts", Some(4), Some(12), "<b1@x>")
            .unwrap();
        db.send_draft(b).unwrap();
        // Moved to another From account.
        let c = db.save_draft(&empty_draft("C")).unwrap();
        db.record_server_draft_saved(c, "acc1", "Drafts", Some(4), Some(13), "<c1@x>")
            .unwrap();
        let mut moved = empty_draft("C");
        moved.id = Some(c);
        moved.account_id = "acc2".into();
        db.save_draft(&moved).unwrap();
        // A mirror recorded with no UIDVALIDITY (no APPENDUID, server silent).
        let e = db.save_draft(&empty_draft("E")).unwrap();
        db.record_server_draft_saved(e, "acc1", "Drafts", None, Some(14), "<e1@x>")
            .unwrap();
        db.delete_draft(e).unwrap();

        let cleanups = ops_of_kind(&db, "acc1", "server_draft_cleanup");
        let by_uid: Vec<(i64, i64)> = cleanups
            .iter()
            .map(|p| {
                (
                    p["uid"].as_i64().unwrap(),
                    p["uid_validity"].as_i64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            by_uid,
            vec![(10, 4), (11, 4), (12, 4), (13, 4), (14, 0)],
            "0 = unknown, which replay never deletes for"
        );
    }

    ///: a UIDVALIDITY reset of Drafts invalidates queued
    /// cleanups AND the UIDs still recorded on draft rows.
    #[test]
    fn uid_space_reset_of_drafts_cancels_cleanups_and_forgets_stale_mirror_uids() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_drafts_role_mailbox(&db, "acc1", "Drafts");
        db.set_sync_cursor(
            "acc1",
            "acc1:Drafts",
            &SyncCursor {
                uid_validity: Some(4),
                uid_next: Some(30),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
        // A pending cleanup of uid 20 in the old UID space.
        let gone = db.save_draft(&empty_draft("Gone")).unwrap();
        db.record_server_draft_saved(gone, "acc1", "Drafts", Some(4), Some(20), "<g@x>")
            .unwrap();
        db.delete_draft(gone).unwrap();
        // A live draft still mirrored at uid 10 in the old UID space.
        let live = db.save_draft(&empty_draft("Live")).unwrap();
        db.record_server_draft_saved(live, "acc1", "Drafts", Some(4), Some(10), "<l@x>")
            .unwrap();
        // One already saved again under the NEW UID space (5).
        let fresh = db.save_draft(&empty_draft("Fresh")).unwrap();
        db.record_server_draft_saved(fresh, "acc1", "Drafts", Some(5), Some(1), "<f@x>")
            .unwrap();

        let cancelled = db
            .purge_mailbox_for_uid_space_reset("acc1", "acc1:Drafts", "Drafts", 5)
            .unwrap();

        assert_eq!(cancelled, 1);
        assert!(ops_of_kind(&db, "acc1", "server_draft_cleanup").is_empty());
        let live = db.get_draft(live).unwrap().unwrap();
        assert_eq!(live.server_uid, None, "uid 10 means nothing in UID space 5");
        assert_eq!(live.server_uid_validity, None);
        assert_eq!(
            live.server_message_id.as_deref(),
            Some("<l@x>"),
            "kept: the old copy is still findable by Message-ID"
        );
        // Discarding it now queues nothing that could delete a stranger.
        db.delete_draft(live.id).unwrap();
        assert!(ops_of_kind(&db, "acc1", "server_draft_cleanup").is_empty());
        let fresh = db.get_draft(fresh).unwrap().unwrap();
        assert_eq!(fresh.server_uid, Some(1), "a mirror in the new space stays");
    }

    #[test]
    fn open_server_draft_hydrates_a_local_row_and_is_idempotent() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_drafts_role_mailbox(&db, "acc1", "Drafts");
        db.upsert_message(&NewMessage {
            id: "acc1:Drafts:9".into(),
            account_id: "acc1".into(),
            mailbox_id: "acc1:Drafts".into(),
            provider_uid: Some(9),
            rfc822_message_id: Some("<other@x>".into()),
            subject: Some("From another client".into()),
            from_name: None,
            from_address: None,
            sent_at: Some(1000),
            snippet: None,
            size_bytes: None,
            has_attachments: false,
            attachments: None,
            is_read: true,
            is_starred: false,
            addresses: vec![Address {
                kind: "to".into(),
                name: None,
                address: "carol@example.com".into(),
            }],
            text_body: Some("hello from another client".into()),
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: vec![],
        })
        .unwrap();

        // The Drafts mailbox was synced in UID space 77; uid 9 belongs to it.
        db.set_sync_cursor(
            "acc1",
            "acc1:Drafts",
            &SyncCursor {
                uid_validity: Some(77),
                uid_next: Some(10),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();

        let id1 = db.open_server_draft("acc1:Drafts:9").unwrap();
        let draft = db.get_draft(id1).unwrap().unwrap();
        assert_eq!(draft.server_uid, Some(9));
        assert_eq!(
            draft.server_uid_validity,
            Some(77),
            "the mirror records the UID space it was read from (#3370)"
        );
        assert_eq!(draft.to[0].email, "carol@example.com");
        assert_eq!(draft.body_text, "hello from another client");

        let id2 = db.open_server_draft("acc1:Drafts:9").unwrap();
        assert_eq!(
            id1, id2,
            "opening the same server message twice must not duplicate the local draft"
        );
    }

    #[test]
    fn reconcile_newest_wins_adopts_a_newer_server_copy_and_keeps_the_loser_locally() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_drafts_role_mailbox(&db, "acc1", "Drafts");
        let mut d = empty_draft("Mine");
        d.body_text = "my edit".into();
        let id = db.save_draft(&d).unwrap();
        // We think we saved uid 5 a long time ago (epoch 100).
        db.record_server_draft_saved(id, "acc1", "Drafts", Some(1), Some(5), "<mine@x>")
            .unwrap();
        {
            let conn = db.lock_conn();
            conn.execute(
                "UPDATE drafts SET updated_at = datetime(100, 'unixepoch') WHERE id = ?1",
                params![id],
            )
            .unwrap();
        }
        // uid 5 is gone; a different, newer message (uid 6, sent_at 5000) is in Drafts now.
        db.upsert_message(&NewMessage {
            id: "acc1:Drafts:6".into(),
            account_id: "acc1".into(),
            mailbox_id: "acc1:Drafts".into(),
            provider_uid: Some(6),
            rfc822_message_id: Some("<theirs@x>".into()),
            subject: Some("Theirs".into()),
            from_name: None,
            from_address: None,
            sent_at: Some(5000),
            snippet: None,
            size_bytes: None,
            has_attachments: false,
            attachments: None,
            is_read: true,
            is_starred: false,
            addresses: vec![],
            text_body: None,
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: vec![],
        })
        .unwrap();

        let changed = db.reconcile_server_draft_conflicts("acc1").unwrap();
        assert_eq!(changed, 1);
        // The loser (our local edit) must survive as an untracked draft,
        // in place: same row, same id (#3437).
        let all = db.list_drafts("acc1").unwrap();
        assert!(
            all.iter().any(|d| d.id == id
                && d.subject == "Mine (kept locally)"
                && d.body_text == "my edit"
                && d.server_uid.is_none()),
            "the loser (our local edit) must survive as an untracked draft: {all:?}"
        );
    }

    /// A synced message in `<account>:Drafts`, as another client would leave
    /// it: hydrated body, no attachments unless the caller adds them.
    fn server_draft_message(account_id: &str, uid: i64, sent_at: i64) -> NewMessage {
        NewMessage {
            id: format!("{account_id}:Drafts:{uid}"),
            account_id: account_id.into(),
            mailbox_id: format!("{account_id}:Drafts"),
            provider_uid: Some(uid),
            rfc822_message_id: Some(format!("<server-{account_id}-{uid}@x>")),
            subject: Some("From another client".into()),
            from_name: None,
            from_address: None,
            sent_at: Some(sent_at),
            snippet: None,
            size_bytes: None,
            has_attachments: false,
            attachments: Some(vec![]),
            is_read: true,
            is_starred: false,
            addresses: vec![],
            text_body: Some("hello from another client".into()),
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: vec![],
        }
    }

    fn pdf_descriptor(ordinal: i64) -> AttachmentDescriptor {
        AttachmentDescriptor {
            ordinal,
            filename: Some(format!("report-{ordinal}.pdf")),
            content_type: Some("application/pdf".into()),
            size_bytes: Some(5),
            section: format!("{}", ordinal + 2),
            content_id: None,
            is_inline: false,
            transfer_encoding: Some("base64".into()),
        }
    }

    fn pending_cleanup_uids(db: &Database, account_id: &str) -> Vec<i64> {
        db.pending_operations(account_id)
            .unwrap()
            .iter()
            .filter(|o| o.kind == "server_draft_cleanup")
            .map(|o| {
                let v: serde_json::Value = serde_json::from_str(&o.payload_json).unwrap();
                v["uid"].as_i64().unwrap()
            })
            .collect()
    }

    // #3437: the conflict arm used to INSERT a lossy copy and DELETE the
    // original row, which cascaded to its attachments and invalidated the id
    // an open compose window was still saving to.
    #[test]
    fn reconcile_conflict_keeps_the_row_its_id_attachments_from_and_html() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_drafts_role_mailbox(&db, "acc1", "Drafts");
        let mut d = empty_draft("Mine");
        d.body_text = "my edit".into();
        d.body_html = Some("<p>my edit</p>".into());
        d.from_email = Some("alias@local.test".into());
        d.from_name = Some("Alias".into());
        let id = db.save_draft(&d).unwrap();
        db.add_draft_attachment(id, "a.pdf", "application/pdf", false, None, b"bytes")
            .unwrap();
        db.record_server_draft_saved(id, "acc1", "Drafts", Some(1), Some(5), "<mine@x>")
            .unwrap();
        {
            let conn = db.lock_conn();
            conn.execute(
                "UPDATE drafts SET updated_at = datetime(100, 'unixepoch') WHERE id = ?1",
                params![id],
            )
            .unwrap();
        }
        // uid 5 is gone; an UNRELATED later message (uid 6) sits in Drafts.
        db.upsert_message(&server_draft_message("acc1", 6, 5000))
            .unwrap();

        assert_eq!(db.reconcile_server_draft_conflicts("acc1").unwrap(), 1);

        let kept = db
            .get_draft(id)
            .unwrap()
            .expect("the draft row and its id must survive a conflict");
        assert_eq!(kept.body_text, "my edit");
        assert_eq!(kept.body_html.as_deref(), Some("<p>my edit</p>"));
        assert_eq!(kept.from_email.as_deref(), Some("alias@local.test"));
        assert_eq!(kept.from_name.as_deref(), Some("Alias"));
        assert!(kept.server_uid.is_none() && kept.server_mailbox_path.is_none());
        let atts = db.list_draft_attachments(id).unwrap();
        assert_eq!(atts.len(), 1, "attachments must survive: {atts:?}");
        assert_eq!(
            db.draft_attachment_bytes(atts[0].id).unwrap().unwrap(),
            b"bytes"
        );
        assert_eq!(db.list_drafts("acc1").unwrap().len(), 1, "no extra copy");

        // The open compose window's next autosave still lands on the same id.
        let mut next = d.clone();
        next.id = Some(id);
        next.body_text = "typed after the sync".into();
        assert_eq!(db.save_draft(&next).unwrap(), id);

        // A second pass has nothing left to do and never re-suffixes.
        assert_eq!(db.reconcile_server_draft_conflicts("acc1").unwrap(), 0);
    }

    // A server message that another local draft already mirrors is that
    // draft's own copy, not a "newer edit" of this one.
    #[test]
    fn reconcile_does_not_treat_another_local_drafts_mirror_as_a_conflict() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_drafts_role_mailbox(&db, "acc1", "Drafts");
        let a = db.save_draft(&empty_draft("A")).unwrap();
        db.record_server_draft_saved(a, "acc1", "Drafts", Some(1), Some(5), "<a@x>")
            .unwrap();
        let b = db.save_draft(&empty_draft("B")).unwrap();
        db.record_server_draft_saved(b, "acc1", "Drafts", Some(1), Some(6), "<b@x>")
            .unwrap();
        {
            let conn = db.lock_conn();
            conn.execute(
                "UPDATE drafts SET updated_at = datetime(100, 'unixepoch')",
                [],
            )
            .unwrap();
        }
        db.upsert_message(&server_draft_message("acc1", 6, 5000))
            .unwrap();

        assert_eq!(db.reconcile_server_draft_conflicts("acc1").unwrap(), 1);
        let a_after = db.get_draft(a).unwrap().unwrap();
        assert_eq!(a_after.subject, "A", "no conflict label: {a_after:?}");
        assert!(a_after.server_uid.is_none());
        assert_eq!(db.get_draft(b).unwrap().unwrap().server_uid, Some(6));
    }

    // #3444: the lookup matched on path + UID alone, so account B's server
    // draft resolved to account A's local row.
    #[test]
    fn open_server_draft_never_returns_another_accounts_draft() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_drafts_role_mailbox(&db, "acc1", "Drafts");
        db.upsert_account(&Account {
            id: "acc2".into(),
            email: "second@local.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        seed_drafts_role_mailbox(&db, "acc2", "Drafts");
        let a = db.save_draft(&empty_draft("A's draft")).unwrap();
        db.record_server_draft_saved(a, "acc1", "Drafts", Some(1), Some(5), "<a@x>")
            .unwrap();
        db.upsert_message(&server_draft_message("acc2", 5, 1000))
            .unwrap();

        let opened = db.open_server_draft("acc2:Drafts:5").unwrap();
        assert_ne!(opened, a, "must not resolve to account A's draft");
        let draft = db.get_draft(opened).unwrap().unwrap();
        assert_eq!(draft.account_id, "acc2");
        assert_eq!(draft.body_text, "hello from another client");
        assert_eq!(db.get_draft(a).unwrap().unwrap().subject, "A's draft");
    }

    // #3436 / #3444: hydration dropped the From identity and the reply
    // threading, so the reply went out unthreaded from the account default.
    #[test]
    fn open_server_draft_hydrates_from_identity_and_reply_threading() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_drafts_role_mailbox(&db, "acc1", "Drafts");
        db.create_identity(&castellum_models::NewIdentity {
            account_id: "acc1".into(),
            email: "alias@local.test".into(),
            display_name: Some("Alias".into()),
            reply_to: None,
            bcc: None,
            is_default: false,
        })
        .unwrap();
        let mut m = server_draft_message("acc1", 9, 1000);
        m.from_address = Some("Alias@Local.Test".into());
        m.from_name = Some("Me As Alias".into());
        // Stored order is In-Reply-To first, then the rest of References.
        m.references = vec!["parent@x".into(), "root@x".into(), "mid@x".into()];
        db.upsert_message(&m).unwrap();

        let id = db.open_server_draft("acc1:Drafts:9").unwrap();
        let draft = db.get_draft(id).unwrap().unwrap();
        // Stored as the From picker would store it: the identity's spelling.
        assert_eq!(draft.from_email.as_deref(), Some("alias@local.test"));
        assert_eq!(draft.from_name.as_deref(), Some("Alias"));
        assert_eq!(draft.in_reply_to.as_deref(), Some("<parent@x>"));
        assert_eq!(
            draft.references_header.as_deref(),
            Some("<root@x> <mid@x> <parent@x>")
        );
        assert_eq!(
            draft.server_uid,
            Some(9),
            "complete hydration stays tracked"
        );
    }

    #[test]
    fn open_server_draft_never_adopts_a_from_that_is_not_one_of_ours() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_drafts_role_mailbox(&db, "acc1", "Drafts");
        let mut m = server_draft_message("acc1", 9, 1000);
        m.from_address = Some("stranger@elsewhere.test".into());
        m.from_name = Some("Stranger".into());
        db.upsert_message(&m).unwrap();

        let draft = db
            .get_draft(db.open_server_draft("acc1:Drafts:9").unwrap())
            .unwrap()
            .unwrap();
        assert!(draft.from_email.is_none() && draft.from_name.is_none());
        assert!(draft.in_reply_to.is_none() && draft.references_header.is_none());
    }

    // #3436: attachments whose bytes are available locally come along, and
    // the hydrated draft keeps tracking (and later replacing) the server copy.
    #[test]
    fn open_server_draft_copies_locally_available_attachments() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_drafts_role_mailbox(&db, "acc1", "Drafts");
        let mut m = server_draft_message("acc1", 9, 1000);
        m.has_attachments = true;
        let mut inline = pdf_descriptor(1);
        inline.is_inline = true;
        inline.content_id = Some("logo123".into());
        m.attachments = Some(vec![pdf_descriptor(0), inline]);
        db.upsert_message(&m).unwrap();
        db.upsert_attachment_cache_entry("acc1:Drafts:9", 0, "hash-0", 3, 1)
            .unwrap();
        db.upsert_attachment_cache_entry("acc1:Drafts:9", 1, "hash-1", 3, 1)
            .unwrap();

        let id = db
            .open_server_draft_with("acc1:Drafts:9", |hash| Some(hash.as_bytes().to_vec()))
            .unwrap();
        let atts = db.list_draft_attachments(id).unwrap();
        assert_eq!(atts.len(), 2, "{atts:?}");
        assert_eq!(atts[0].filename, "report-0.pdf");
        assert_eq!(atts[0].content_type, "application/pdf");
        assert!(!atts[0].is_inline);
        assert!(atts[1].is_inline);
        assert_eq!(atts[1].content_id.as_deref(), Some("logo123"));
        assert_eq!(
            db.draft_attachment_bytes(atts[0].id).unwrap().unwrap(),
            b"hash-0"
        );
        assert_eq!(db.get_draft(id).unwrap().unwrap().server_uid, Some(9));

        // Idempotent: a second open neither duplicates the row nor the files.
        let again = db
            .open_server_draft_with("acc1:Drafts:9", |hash| Some(hash.as_bytes().to_vec()))
            .unwrap();
        assert_eq!(again, id);
        assert_eq!(db.list_draft_attachments(id).unwrap().len(), 2);
    }

    // #3436, the destructive half: a draft we could NOT hydrate completely
    // must never cause the original server copy (the only one that still has
    // the attachment) to be expunged by autosave, discard or send.
    #[test]
    fn open_server_draft_with_an_unavailable_attachment_never_expunges_the_original() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_drafts_role_mailbox(&db, "acc1", "Drafts");
        let mut m = server_draft_message("acc1", 9, 1000);
        m.has_attachments = true;
        m.attachments = Some(vec![pdf_descriptor(0)]);
        db.upsert_message(&m).unwrap();

        // Not cached at all (plain `open_server_draft` has no byte source).
        let id = db.open_server_draft("acc1:Drafts:9").unwrap();
        let draft = db.get_draft(id).unwrap().unwrap();
        assert_eq!(draft.body_text, "hello from another client");
        assert!(
            draft.server_uid.is_none(),
            "an incomplete hydration must not track the original's UID: {draft:?}"
        );
        assert_eq!(
            db.open_server_draft("acc1:Drafts:9").unwrap(),
            id,
            "still idempotent while untracked"
        );

        // The autosave's APPEND lands as uid 20: the original uid 9 stays.
        db.record_server_draft_saved(id, "acc1", "Drafts", Some(1), Some(20), "<new@x>")
            .unwrap();
        assert!(pending_cleanup_uids(&db, "acc1").is_empty());
        // Discarding cleans up only our own copy.
        db.delete_draft(id).unwrap();
        assert_eq!(pending_cleanup_uids(&db, "acc1"), vec![20]);
    }

    #[test]
    fn open_server_draft_stays_untracked_when_a_cached_file_is_missing_or_body_is_html_only() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_drafts_role_mailbox(&db, "acc1", "Drafts");
        // uid 9: one cache row's file is gone.
        let mut m = server_draft_message("acc1", 9, 1000);
        m.has_attachments = true;
        m.attachments = Some(vec![pdf_descriptor(0), pdf_descriptor(1)]);
        db.upsert_message(&m).unwrap();
        db.upsert_attachment_cache_entry("acc1:Drafts:9", 0, "present", 3, 1)
            .unwrap();
        db.upsert_attachment_cache_entry("acc1:Drafts:9", 1, "gone", 3, 1)
            .unwrap();
        let id = db
            .open_server_draft_with("acc1:Drafts:9", |hash| {
                (hash == "present").then(|| b"abc".to_vec())
            })
            .unwrap();
        assert!(db.get_draft(id).unwrap().unwrap().server_uid.is_none());
        assert_eq!(
            db.list_draft_attachments(id).unwrap().len(),
            1,
            "what is available is still carried over"
        );

        // uid 10: HTML-only body. The plain-text composer would open it blank
        // and the first autosave would replace the real content.
        let mut html_only = server_draft_message("acc1", 10, 1000);
        html_only.text_body = None;
        html_only.html_body = Some("<p>rich</p>".into());
        db.upsert_message(&html_only).unwrap();
        let id = db.open_server_draft("acc1:Drafts:10").unwrap();
        assert!(db.get_draft(id).unwrap().unwrap().server_uid.is_none());

        // uid 11: envelope only, body never fetched.
        let mut envelope_only = server_draft_message("acc1", 11, 1000);
        envelope_only.text_body = None;
        envelope_only.attachments = None;
        db.upsert_message(&envelope_only).unwrap();
        let id = db.open_server_draft("acc1:Drafts:11").unwrap();
        assert!(db.get_draft(id).unwrap().unwrap().server_uid.is_none());
    }

    #[test]
    fn reconcile_clears_a_vanished_uid_with_no_newer_candidate() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_drafts_role_mailbox(&db, "acc1", "Drafts");
        let id = db.save_draft(&empty_draft("Mine")).unwrap();
        db.record_server_draft_saved(id, "acc1", "Drafts", Some(1), Some(5), "<mine@x>")
            .unwrap();
        // uid 5 vanished (our own cleanup, not yet reflected) and nothing newer exists.
        let changed = db.reconcile_server_draft_conflicts("acc1").unwrap();
        assert_eq!(changed, 1);
        assert!(db.get_draft(id).unwrap().unwrap().server_uid.is_none());
    }

    #[test]
    fn save_draft_inserts_updates_and_round_trips() {
        let db = fresh_db();
        seed_account_mailbox(&db);

        let id = db.save_draft(&draft_input("First")).unwrap();
        assert!(id > 0);

        // Round-trip: addresses, headers and status come back intact.
        let d = db.get_draft(id).unwrap().expect("draft exists");
        assert_eq!(d.subject, "First");
        assert_eq!(d.status, "draft");
        assert_eq!(d.to[0].email, "bob@local.test");
        assert_eq!(d.to[0].name.as_deref(), Some("Bob"));
        assert_eq!(d.cc[0].email, "carol@local.test");
        assert_eq!(d.bcc[0].email, "dave@local.test");
        assert_eq!(d.in_reply_to.as_deref(), Some("<parent@local.test>"));

        // Update in place: same id, new content.
        let mut upd = draft_input("Second");
        upd.id = Some(id);
        upd.bcc = vec![];
        assert_eq!(db.save_draft(&upd).unwrap(), id);
        let d2 = db.get_draft(id).unwrap().unwrap();
        assert_eq!(d2.subject, "Second");
        assert!(d2.bcc.is_empty());

        // list_drafts returns it; a second draft lists too.
        db.save_draft(&draft_input("Third")).unwrap();
        let all = db.list_drafts("acc1").unwrap();
        assert_eq!(all.len(), 2);

        // Unknown ids: get -> None, update -> clean error.
        assert!(db.get_draft(999).unwrap().is_none());
        let mut ghost = draft_input("Ghost");
        ghost.id = Some(999);
        assert!(db.save_draft(&ghost).is_err());
    }

    #[test]
    fn delete_draft_only_while_draft() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let id = db.save_draft(&draft_input("Doomed")).unwrap();
        db.delete_draft(id).unwrap();
        assert!(db.get_draft(id).unwrap().is_none());
        // Deleting again (unknown id) errors cleanly.
        assert!(db.delete_draft(id).is_err());

        // An outbox draft is refused: it's owned by the send queue.
        let id2 = db.save_draft(&draft_input("Queued")).unwrap();
        db.send_draft(id2).unwrap();
        let err = db.delete_draft(id2).unwrap_err();
        assert!(err.to_string().contains("outbox"), "got: {err}");
        assert!(db.get_draft(id2).unwrap().is_some(), "still there");
    }

    #[test]
    fn send_draft_queues_op_and_guards_state() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        let id = db.save_draft(&draft_input("Outgoing")).unwrap();

        db.send_draft(id).unwrap();
        // Status flipped + exactly one `send` op enqueued, atomically.
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "outbox");
        let ops = db.pending_operations("acc1").unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].kind, "send");
        let payload: serde_json::Value = serde_json::from_str(&ops[0].payload_json).unwrap();
        assert_eq!(payload["draft_id"].as_i64(), Some(id));

        // Sending again (already outbox) fails cleanly and enqueues nothing.
        let err = db.send_draft(id).unwrap_err();
        assert!(err.to_string().contains("outbox"), "got: {err}");
        assert_eq!(db.pending_operations("acc1").unwrap().len(), 1);

        // A missing draft fails cleanly too.
        assert!(db.send_draft(12345).is_err());

        // A queued draft is no longer editable.
        let mut upd = draft_input("Too late");
        upd.id = Some(id);
        assert!(db.save_draft(&upd).is_err());

        // mark_draft_sent completes the lifecycle.
        db.mark_draft_sent(id).unwrap();
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "sent");
        assert!(db.mark_draft_sent(999).is_err());
    }

    #[test]
    fn mark_draft_sent_with_copy_enqueues_op_and_autocreates_sent() {
        let db = fresh_db();
        seed_account_mailbox(&db); // no role='sent' mailbox seeded
        let id = db.save_draft(&draft_input("Copy me")).unwrap();
        db.send_draft(id).unwrap();

        db.mark_draft_sent_with_copy(id, "<mid-1@x>", b"raw rfc822 bytes")
            .unwrap();

        // Draft delivered + a local Sent mailbox now exists (default path).
        assert_eq!(db.get_draft(id).unwrap().unwrap().status, "sent");
        let sent = db
            .list_mailboxes("acc1")
            .unwrap()
            .into_iter()
            .find(|m| m.role.as_deref() == Some("sent"))
            .expect("Sent mailbox auto-created");
        assert_eq!(sent.path, "Sent");

        // Exactly one sent_copy op. The payload holds only small routing
        // fields; the delivered bytes live in operation_blobs by op id.
        let ops = db.pending_operations("acc1").unwrap();
        let copy = ops
            .iter()
            .find(|o| o.kind == "sent_copy")
            .expect("a sent_copy op");
        let payload: serde_json::Value = serde_json::from_str(&copy.payload_json).unwrap();
        assert_eq!(payload["dest_path"], "Sent");
        assert_eq!(payload["message_id"], "<mid-1@x>");
        assert!(
            payload.get("raw_b64").is_none(),
            "bytes must not be inlined in the op payload"
        );
        assert_eq!(
            db.operation_blob(copy.id).unwrap().as_deref(),
            Some(b"raw rfc822 bytes".as_slice())
        );

        // Terminal success reclaims the blob; the op row stays for audit.
        db.set_operation_status(copy.id, "succeeded", None).unwrap();
        assert_eq!(db.operation_blob(copy.id).unwrap(), None);

        // Unknown draft id fails cleanly (and enqueues nothing).
        assert!(db.mark_draft_sent_with_copy(999, "<x@y>", b"z").is_err());
    }

    #[test]
    fn mark_draft_sent_with_copy_reuses_existing_sent_mailbox() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        // Server-flavoured Sent folder (e.g. Outlook) already known locally.
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Sent Items".into(),
            account_id: "acc1".into(),
            path: "Sent Items".into(),
            name: "Sent Items".into(),
            role: Some("sent".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        let id = db.save_draft(&draft_input("Reuse")).unwrap();
        db.send_draft(id).unwrap();
        db.mark_draft_sent_with_copy(id, "<mid-2@x>", b"bytes")
            .unwrap();

        let copy = db
            .pending_operations("acc1")
            .unwrap()
            .into_iter()
            .find(|o| o.kind == "sent_copy")
            .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&copy.payload_json).unwrap();
        assert_eq!(payload["dest_path"], "Sent Items", "no second Sent created");
        // And no extra mailbox row appeared.
        let sent_boxes: Vec<_> = db
            .list_mailboxes("acc1")
            .unwrap()
            .into_iter()
            .filter(|m| m.role.as_deref() == Some("sent"))
            .collect();
        assert_eq!(sent_boxes.len(), 1);
    }

    #[test]
    fn sent_copy_survives_uidvalidity_purge() {
        // A UIDVALIDITY reset on the Sent mailbox purges its local rows and
        // cancels stale UID-based ops — but a queued sent_copy carries raw
        // bytes and a destination path, not UIDs: the APPEND is still valid
        // and must survive.
        let db = fresh_db();
        seed_account_mailbox(&db);
        let id = db.save_draft(&draft_input("Survivor")).unwrap();
        db.send_draft(id).unwrap();
        db.mark_draft_sent_with_copy(id, "<mid-3@x>", b"bytes")
            .unwrap();

        let cancelled = db
            .cancel_operations_for_purged_mailbox("acc1", "acc1:Sent", "Sent", "cancelled: test")
            .unwrap();
        assert_eq!(cancelled, 0, "sent_copy must not be cancelled");
        let ops = db.pending_operations("acc1").unwrap();
        assert!(
            ops.iter().any(|o| o.kind == "sent_copy"),
            "sent_copy still pending after the purge"
        );
    }

    // --- trust surface (S48) -----------------------------------------------

    #[test]
    fn upsert_message_trust_round_trips_through_get_message_detail() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "s", 1000)).unwrap();
        let trust = TrustSignals {
            spf: TrustResult::Pass,
            dkim: TrustResult::Fail,
            dmarc: TrustResult::None,
            authserv_id: Some("imap.fastmail.com".into()),
        };
        db.upsert_message_trust("acc1:INBOX:1", &trust).unwrap();

        let detail = db.get_message_detail("acc1:INBOX:1").unwrap().unwrap();
        assert_eq!(detail.trust, trust);
    }

    #[test]
    fn message_with_no_trust_row_reads_back_all_none() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "s", 1000)).unwrap();

        let detail = db.get_message_detail("acc1:INBOX:1").unwrap().unwrap();
        assert_eq!(detail.trust, TrustSignals::default());
    }

    #[test]
    fn upsert_message_trust_overwrites_on_resync() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "s", 1000)).unwrap();
        db.upsert_message_trust(
            "acc1:INBOX:1",
            &TrustSignals {
                spf: TrustResult::Fail,
                ..Default::default()
            },
        )
        .unwrap();
        db.upsert_message_trust(
            "acc1:INBOX:1",
            &TrustSignals {
                spf: TrustResult::Pass,
                ..Default::default()
            },
        )
        .unwrap();

        let detail = db.get_message_detail("acc1:INBOX:1").unwrap().unwrap();
        assert_eq!(detail.trust.spf, TrustResult::Pass);
    }

    #[test]
    fn sender_remote_content_policy_set_get_list_revoke() {
        let db = fresh_db();
        db.upsert_account(&Account {
            id: "acc1".into(),
            email: "acc1@local.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();

        assert_eq!(
            db.sender_remote_content_allowed("acc1", "News@Example.com")
                .unwrap(),
            None,
            "no policy yet"
        );

        db.set_sender_remote_content_allowed("acc1", "News@Example.com", true)
            .unwrap();
        assert_eq!(
            db.sender_remote_content_allowed("acc1", "news@example.com")
                .unwrap(),
            Some(true),
            "lookup is case-insensitive"
        );

        let all = db.list_sender_remote_content_policies().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(
            all[0].from_address, "news@example.com",
            "stored lower-cased"
        );
        assert!(all[0].allowed);

        db.revoke_sender_remote_content("acc1", "news@example.com")
            .unwrap();
        assert_eq!(
            db.sender_remote_content_allowed("acc1", "news@example.com")
                .unwrap(),
            None
        );
        assert!(db.list_sender_remote_content_policies().unwrap().is_empty());
    }

    #[test]
    fn revoking_a_policy_that_does_not_exist_is_not_an_error() {
        let db = fresh_db();
        db.upsert_account(&Account {
            id: "acc1".into(),
            email: "acc1@local.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.revoke_sender_remote_content("acc1", "nobody@example.com")
            .unwrap();
    }

    #[test]
    fn spotlight_indexing_defaults_off_and_round_trips() {
        let db = fresh_db();
        assert!(
            !db.spotlight_indexing_enabled().unwrap(),
            "a fresh profile must default OFF — Spotlight indexing is opt-in"
        );

        db.set_spotlight_indexing_enabled(true).unwrap();
        assert!(db.spotlight_indexing_enabled().unwrap());

        db.set_spotlight_indexing_enabled(false).unwrap();
        assert!(!db.spotlight_indexing_enabled().unwrap());
    }

    #[test]
    fn spotlight_indexing_flag_persists_across_reopen() {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-repo-test-spotlight-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);

        {
            let db = Database::open(&path).expect("open");
            assert!(!db.spotlight_indexing_enabled().unwrap());
            db.set_spotlight_indexing_enabled(true).unwrap();
        }
        {
            let db = Database::open(&path).expect("reopen");
            assert!(
                db.spotlight_indexing_enabled().unwrap(),
                "the opt-in is durable — it lives in app_meta, not in-memory state"
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    // --- QRESYNC cursor invalidation on local-only flag changes (S30 round 3) --
    //
    // QRESYNC only reports what the SERVER changed. A local flag that diverges
    // from the server while the server stays put — an evaluation-mode
    // auto-read (no op queued), or an op the user cancels after its local
    // flag was already applied — is invisible to it, so the mailbox's
    // HIGHESTMODSEQ must be invalidated to force one full scan that converges
    // it. Invalidated = a fresh non-positive value (never QRESYNC-eligible).

    fn seed_inbox_cursor(db: &Database, modseq: Option<i64>) {
        db.set_sync_cursor(
            "acc1",
            "acc1:INBOX",
            &SyncCursor {
                uid_validity: Some(7),
                uid_next: Some(100),
                highest_modseq: modseq,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
    }

    fn inbox_cursor(db: &Database) -> SyncCursor {
        db.get_sync_cursor("acc1", "acc1:INBOX").unwrap().unwrap()
    }

    fn qresync_eligible(c: &SyncCursor) -> bool {
        c.highest_modseq.is_some_and(|m| m > 0)
    }

    #[test]
    fn an_evaluation_mode_auto_read_invalidates_the_mailbox_modseq_only_when_it_changes() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.set_account_write_enabled("acc1", false).unwrap();
        db.upsert_message(&msg(42, "s", 1)).unwrap();
        seed_inbox_cursor(&db, Some(500));

        db.mark_read_auto("acc1:INBOX:42", true).unwrap();
        let after = inbox_cursor(&db);
        assert!(
            !qresync_eligible(&after),
            "a local-only read flip must force a full scan: {after:?}"
        );
        assert_eq!((after.uid_validity, after.uid_next), (Some(7), Some(100)));

        // Re-opening an already-read message changes nothing locally, so it
        // must not keep re-invalidating the cursor.
        db.mark_read_auto("acc1:INBOX:42", true).unwrap();
        assert_eq!(inbox_cursor(&db), after);
    }

    #[test]
    fn an_auto_read_that_queues_an_op_leaves_the_modseq_alone() {
        let db = fresh_db();
        seed_account_mailbox(&db); // write-enabled: the op carries the change
        db.upsert_message(&msg(42, "s", 1)).unwrap();
        seed_inbox_cursor(&db, Some(500));
        db.mark_read_auto("acc1:INBOX:42", true).unwrap();
        assert_eq!(inbox_cursor(&db).highest_modseq, Some(500));
    }

    /// Adds an Archive mailbox (path "Archive") to the seeded account.
    fn seed_archive_mailbox(db: &Database) {
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Archive".into(),
            account_id: "acc1".into(),
            path: "Archive".into(),
            name: "Archive".into(),
            role: Some("archive".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
    }

    fn keywords_of(db: &Database, id: &str) -> Vec<(String, String)> {
        let conn = db.lock_conn();
        let mut stmt = conn
            .prepare(
                "SELECT keyword_key, origin FROM message_keywords WHERE message_id = ?1 ORDER BY 1",
            )
            .unwrap();
        stmt.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    // #3438: archive/delete/move deleted the source row and cascaded away
    // its Mac-only (origin='local') tags; the arrival came back bare.
    #[test]
    fn mac_only_tags_survive_archive_when_the_message_arrives_in_the_destination() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_archive_mailbox(&db);
        db.upsert_message(&msg(7, "tagged", 1)).unwrap();
        db.set_message_keyword("acc1:INBOX:7", "Work", true)
            .unwrap();
        db.set_message_keyword("acc1:INBOX:7", "Synced", true)
            .unwrap();
        // PERMANENTFLAGS refused Work: replay demoted it to Mac-only.
        assert!(db.demote_keyword_to_local("acc1:INBOX:7", "Work").unwrap());

        db.archive_message("acc1:INBOX:7").unwrap();

        // The arrival (new id, same Message-ID) reports only server keywords.
        let mut arrival = msg(70, "tagged", 1);
        arrival.id = "acc1:Archive:70".into();
        arrival.mailbox_id = "acc1:Archive".into();
        arrival.rfc822_message_id = Some("<7@local.test>".into());
        db.upsert_message(&arrival).unwrap();
        assert_eq!(
            keywords_of(&db, "acc1:Archive:70"),
            vec![("work".to_string(), "local".to_string())],
            "the Mac-only tag follows the message; the server tag is the server's to report"
        );

        // Consumed once: a duplicate copy does not inherit it.
        let mut dup = arrival.clone();
        dup.id = "acc1:Archive:71".into();
        dup.provider_uid = Some(71);
        db.upsert_message(&dup).unwrap();
        assert!(keywords_of(&db, "acc1:Archive:71").is_empty());
    }

    // #3457: an evaluation-mode tag change is local-only and queues no op, so
    // a QRESYNC delta never reports the (unchanged) server message.
    #[test]
    fn evaluation_mode_tag_removal_or_overwrite_forces_a_full_scan() {
        for add in [false, true] {
            let db = fresh_db();
            seed_account_mailbox(&db);
            db.upsert_message(&msg(42, "s", 1)).unwrap();
            db.replace_server_keywords("acc1:INBOX:42", &["Work".to_string()])
                .unwrap();
            db.set_account_write_enabled("acc1", false).unwrap();
            seed_inbox_cursor(&db, Some(500));
            db.set_message_keyword("acc1:INBOX:42", "Work", add)
                .unwrap();
            assert!(
                !qresync_eligible(&inbox_cursor(&db)),
                "eval-mode tag change (add={add}) must force a full scan"
            );
        }
    }

    #[test]
    fn cancelling_a_keyword_op_invalidates_its_mailbox_modseq() {
        for add in [true, false] {
            let db = fresh_db();
            seed_account_mailbox(&db);
            db.upsert_message(&msg(42, "s", 1)).unwrap();
            if !add {
                db.set_message_keyword("acc1:INBOX:42", "Work", true)
                    .unwrap();
                let op = db.pending_operations("acc1").unwrap()[0].id;
                assert!(db.cancel_operation(op).unwrap());
            }
            seed_inbox_cursor(&db, Some(500));
            db.set_message_keyword("acc1:INBOX:42", "Work", add)
                .unwrap();
            assert_eq!(inbox_cursor(&db).highest_modseq, Some(500));
            let ops = db.pending_operations("acc1").unwrap();
            let op = ops.last().unwrap().id;
            assert!(db.cancel_operation(op).unwrap());
            assert!(
                !qresync_eligible(&inbox_cursor(&db)),
                "cancelled keyword op (add={add}) leaves the tag unreplayed: force a full scan"
            );
        }
    }

    fn arrival_in(m: NewMessage, mailbox: &str, uid: i64, rfc: &str) -> NewMessage {
        let mut a = m;
        a.id = format!("acc1:{mailbox}:{uid}");
        a.mailbox_id = format!("acc1:{mailbox}");
        a.provider_uid = Some(uid);
        a.rfc822_message_id = Some(rfc.into());
        a
    }

    #[test]
    fn a_batch_upsert_reattaches_each_arrivals_own_stash() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        seed_archive_mailbox(&db);
        for (uid, tag) in [(7, "Alpha"), (8, "Beta")] {
            db.upsert_message(&msg(uid, "tagged", 1)).unwrap();
            let id = format!("acc1:INBOX:{uid}");
            db.set_message_keyword(&id, tag, true).unwrap();
            assert!(db.demote_keyword_to_local(&id, tag).unwrap());
            db.archive_message(&id).unwrap();
        }
        // Arrive in the opposite order, in one batch.
        let b = arrival_in(msg(8, "tagged", 1), "Archive", 80, "<8@local.test>");
        let a = arrival_in(msg(7, "tagged", 1), "Archive", 70, "<7@local.test>");
        db.upsert_messages_batch(&[b, a]).unwrap();
        assert_eq!(
            keywords_of(&db, "acc1:Archive:70"),
            vec![("alpha".to_string(), "local".to_string())]
        );
        assert_eq!(
            keywords_of(&db, "acc1:Archive:80"),
            vec![("beta".to_string(), "local".to_string())]
        );
    }

    #[test]
    fn mac_only_tags_return_when_the_move_was_cancelled_or_failed_permanently() {
        for terminal in ["cancelled", "failed_permanent"] {
            let db = fresh_db();
            seed_account_mailbox(&db);
            seed_archive_mailbox(&db);
            db.upsert_message(&msg(7, "tagged", 1)).unwrap();
            db.set_message_keyword("acc1:INBOX:7", "Work", true)
                .unwrap();
            assert!(db.demote_keyword_to_local("acc1:INBOX:7", "Work").unwrap());
            db.archive_message("acc1:INBOX:7").unwrap();
            db.lock_conn()
                .execute(
                    "UPDATE operations SET status = ?1 WHERE kind = 'move'",
                    params![terminal],
                )
                .unwrap();

            // The message never left INBOX: it is re-imported there.
            let back = arrival_in(msg(7, "tagged", 1), "INBOX", 77, "<7@local.test>");
            db.upsert_message(&back).unwrap();
            assert_eq!(
                keywords_of(&db, "acc1:INBOX:77"),
                vec![("work".to_string(), "local".to_string())],
                "{terminal}: the stash returns to the source folder"
            );
        }
    }

    #[test]
    fn cancelling_a_flag_bearing_op_invalidates_its_mailbox_modseq() {
        for kind in ["mark_read", "flag", "mark_all_read"] {
            let db = fresh_db();
            seed_account_mailbox(&db);
            db.upsert_message(&msg(42, "s", 1)).unwrap();
            seed_inbox_cursor(&db, Some(500));
            match kind {
                "mark_read" => db.mark_read("acc1:INBOX:42", true).unwrap(),
                "flag" => db.set_flagged("acc1:INBOX:42", true).unwrap(),
                _ => {
                    db.mark_all_read("acc1", "acc1:INBOX").unwrap();
                }
            }
            assert_eq!(inbox_cursor(&db).highest_modseq, Some(500));
            let op = db.pending_operations("acc1").unwrap()[0].id;
            assert!(db.cancel_operation(op).unwrap());
            assert!(
                !qresync_eligible(&inbox_cursor(&db)),
                "cancelling a {kind} op leaves its local flag unreplayed: force a full scan"
            );
        }
    }

    #[test]
    fn the_sync_cursor_compare_and_set_never_overwrites_a_concurrent_invalidation() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.set_account_write_enabled("acc1", false).unwrap();
        db.upsert_message(&msg(42, "s", 1)).unwrap();
        seed_inbox_cursor(&db, Some(500));
        let advanced = SyncCursor {
            uid_validity: Some(7),
            uid_next: Some(120),
            highest_modseq: Some(600),
            descriptor_checkpoint_uid: None,
        };

        // A sync that read 500 and finds 500 still there may advance it.
        assert!(db
            .set_sync_cursor_if_modseq_is("acc1", "acc1:INBOX", &advanced, Some(500))
            .unwrap());
        assert_eq!(inbox_cursor(&db), advanced);

        // A local-only change lands while a sync that read 600 is running...
        db.mark_read_auto("acc1:INBOX:42", true).unwrap();
        let invalidated = inbox_cursor(&db).highest_modseq;
        // ...so that sync's write keeps the invalidation (UIDs still move).
        let later = SyncCursor {
            uid_next: Some(130),
            highest_modseq: Some(700),
            ..advanced.clone()
        };
        assert!(!db
            .set_sync_cursor_if_modseq_is("acc1", "acc1:INBOX", &later, Some(600))
            .unwrap());
        let now = inbox_cursor(&db);
        assert_eq!(now.highest_modseq, invalidated);
        assert_eq!(now.uid_next, Some(130));

        // A second invalidation must still be distinguishable from the first,
        // or a sync that read the first could not notice the second.
        db.mark_read_auto("acc1:INBOX:42", false).unwrap();
        assert_ne!(inbox_cursor(&db).highest_modseq, invalidated);

        // A first-ever cursor (no row yet) is simply inserted.
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Archive".into(),
            account_id: "acc1".into(),
            path: "Archive".into(),
            name: "Archive".into(),
            role: None,
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        assert!(db
            .set_sync_cursor_if_modseq_is("acc1", "acc1:Archive", &advanced, None)
            .unwrap());
        assert_eq!(
            db.get_sync_cursor("acc1", "acc1:Archive").unwrap(),
            Some(advanced)
        );
    }

    // --- S73 /: Delete inside Trash = permanent delete ----------

    fn message_row_exists(db: &Database, id: &str) -> bool {
        db.lock_conn()
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE id = ?1",
                params![id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            > 0
    }

    /// Seeds INBOX (uid 7) and a Trash mailbox (cursor UIDVALIDITY 42) with
    /// one message in each; returns the ids (inbox, trash).
    fn seed_inbox_and_trash_message(db: &Database) -> (String, String) {
        seed_account_mailbox(db);
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Trash".into(),
            account_id: "acc1".into(),
            path: "Trash".into(),
            name: "Trash".into(),
            role: Some("trash".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.set_sync_cursor(
            "acc1",
            "acc1:Trash",
            &SyncCursor {
                uid_validity: Some(42),
                uid_next: Some(10),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
        db.upsert_message(&msg(7, "inbox one", 1000)).unwrap();
        let mut t = msg(9, "trashed one", 1000);
        t.id = "acc1:Trash:9".into();
        t.mailbox_id = "acc1:Trash".into();
        db.upsert_message(&t).unwrap();
        ("acc1:INBOX:7".into(), "acc1:Trash:9".into())
    }

    #[test]
    fn delete_message_permanently_queues_one_snapshotted_op_and_keeps_the_row() {
        let db = fresh_db();
        let (_, trash_msg) = seed_inbox_and_trash_message(&db);
        db.delete_message_permanently(&trash_msg).unwrap();
        let ops = db.pending_operations("acc1").unwrap();
        let dp: Vec<_> = ops
            .iter()
            .filter(|o| o.kind == "delete_permanent")
            .collect();
        assert_eq!(dp.len(), 1, "{ops:?}");
        let p: serde_json::Value = serde_json::from_str(&dp[0].payload_json).unwrap();
        assert_eq!(p["path"], "Trash");
        assert_eq!(p["uid_validity"], 42);
        assert_eq!(p["uids"], "9");
        assert_eq!(p["message_id"], trash_msg);
        // The row stays until the server confirms: a failed expunge must
        // leave the message visible.
        assert!(message_row_exists(&db, &trash_msg));
        // Pressing Delete twice does not queue a second op.
        db.delete_message_permanently(&trash_msg).unwrap();
        let again = db.pending_operations("acc1").unwrap();
        assert_eq!(
            again
                .iter()
                .filter(|o| o.kind == "delete_permanent")
                .count(),
            1
        );
    }

    #[test]
    fn delete_message_permanently_refuses_a_message_outside_trash() {
        let db = fresh_db();
        let (inbox_msg, _) = seed_inbox_and_trash_message(&db);
        let err = db.delete_message_permanently(&inbox_msg).unwrap_err();
        assert!(err.0.contains("not in Trash"), "{err:?}");
        assert!(db.pending_operations("acc1").unwrap().is_empty());
        assert!(message_row_exists(&db, &inbox_msg));
    }

    #[test]
    fn delete_message_from_trash_never_queues_a_move() {
        let db = fresh_db();
        let (_, trash_msg) = seed_inbox_and_trash_message(&db);
        assert!(db.delete_message(&trash_msg).is_err());
        assert!(db.pending_operations("acc1").unwrap().is_empty());
        // Elsewhere, Delete still moves to Trash.
        db.delete_message("acc1:INBOX:7").unwrap();
        let ops = db.pending_operations("acc1").unwrap();
        assert_eq!(ops.iter().filter(|o| o.kind == "move").count(), 1);
    }

    /// #3459: archiving a message that already sits in its Archive mailbox (or moving it to
    /// its own mailbox) queues no op and keeps the row; Inbox -> Archive still moves.
    #[test]
    fn archiving_a_message_already_in_archive_is_a_noop() {
        let db = fresh_db();
        seed_inbox_and_trash_message(&db);
        db.upsert_mailbox(&Mailbox {
            id: "acc1:Archive".into(),
            account_id: "acc1".into(),
            path: "Archive".into(),
            name: "Archive".into(),
            role: Some("archive".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        let mut a = msg(11, "archived one", 1000);
        a.id = "acc1:Archive:11".into();
        a.mailbox_id = "acc1:Archive".into();
        db.upsert_message(&a).unwrap();

        db.archive_message("acc1:Archive:11").unwrap();
        db.move_message_to_mailbox("acc1:Archive:11", "acc1:Archive")
            .unwrap();
        assert!(db.pending_operations("acc1").unwrap().is_empty());
        assert!(message_row_exists(&db, "acc1:Archive:11"));

        db.archive_message("acc1:INBOX:7").unwrap();
        let ops = db.pending_operations("acc1").unwrap();
        assert_eq!(ops.iter().filter(|o| o.kind == "move").count(), 1);
        assert!(!message_row_exists(&db, "acc1:INBOX:7"));
    }

    #[test]
    fn finish_permanent_delete_removes_the_local_row() {
        let db = fresh_db();
        let (_, trash_msg) = seed_inbox_and_trash_message(&db);
        db.finish_permanent_delete(&trash_msg).unwrap();
        assert!(!message_row_exists(&db, &trash_msg));
    }

    /// Cache a message-scope summary + a job for `mid`; returns the job id.
    fn cache_summary(db: &Database, mid: &str) -> i64 {
        let job = db
            .enqueue_ai_job(
                Some("acc1"),
                "summarize_message",
                &format!(r#"{{"message_id":"{mid}"}}"#),
                0,
            )
            .unwrap();
        db.upsert_ai_output(
            job,
            "message",
            mid,
            "summarize_message",
            "old summary",
            true,
            None,
            None,
            None,
        )
        .unwrap();
        job
    }

    /// #3440: a message that left `messages` through a path other than the
    /// mailbox purge (permanent delete, Empty Trash, move) must not leave a
    /// cached summary behind for the reusable `<mailbox>:<uid>` id.
    #[test]
    fn every_message_removal_path_drops_the_cached_summary() {
        // finish_permanent_delete
        let db = fresh_db();
        let (_, trash_msg) = seed_inbox_and_trash_message(&db);
        cache_summary(&db, &trash_msg);
        db.finish_permanent_delete(&trash_msg).unwrap();
        db.delete_messages_for_mailbox("acc1", "acc1:Trash")
            .unwrap();
        assert!(db
            .get_ai_output("message", &trash_msg, "summarize_message")
            .unwrap()
            .is_none());

        // empty_trash
        let db = fresh_db();
        let (_, trash_msg) = seed_inbox_and_trash_message(&db);
        cache_summary(&db, &trash_msg);
        db.empty_trash("acc1").unwrap();
        db.delete_messages_for_mailbox("acc1", "acc1:Trash")
            .unwrap();
        assert!(db
            .get_ai_output("message", &trash_msg, "summarize_message")
            .unwrap()
            .is_none());

        // enqueue_move (delete_message moves to Trash)
        let db = fresh_db();
        let (inbox_msg, _) = seed_inbox_and_trash_message(&db);
        cache_summary(&db, &inbox_msg);
        db.delete_message(&inbox_msg).unwrap();
        db.delete_messages_for_mailbox("acc1", "acc1:INBOX")
            .unwrap();
        assert!(db
            .get_ai_output("message", &inbox_msg, "summarize_message")
            .unwrap()
            .is_none());
    }

    /// #3440: purging a message whose job the executor holds must not delete
    /// the row (its later event/status writes would fail and abort the pass);
    /// it flags the job for cancellation instead.
    #[test]
    fn purging_a_message_flags_its_in_flight_job_instead_of_deleting_it() {
        let db = fresh_db();
        seed_account_mailbox(&db);
        db.upsert_message(&msg(1, "x", 1000)).unwrap();
        let job = cache_summary(&db, "acc1:INBOX:1");
        db.lease_ai_job(job).unwrap();
        db.purge_message("acc1", "acc1:INBOX:1").unwrap();
        assert!(
            db.is_ai_cancel_requested(job).unwrap(),
            "in-flight job is flagged, not deleted"
        );
        assert!(db.poll_ai_job(job, 0).unwrap().is_some());
    }
}
