// Castellum - read/query APIs (JSON over the FFI).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Thin serialization layer over the db repository. Returns JSON strings so the
// Swift UI consumes them via Codable. Read-only; no mutation.

use crate::{CoreError, Profile};
use serde::Serialize;

/// Wall-clock Unix seconds (0 on a clock failure, which only makes a VIP's
/// `created_at` or an outbox timestamp look ancient — never a panic).
fn wall_clock_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn to_json<T: Serialize>(value: &T) -> Result<String, CoreError> {
    serde_json::to_string(value).map_err(|_| CoreError::Io)
}

fn db_io(e: castellum_db::DbError) -> CoreError {
    CoreError::Db(e.to_string())
}

/// Parse an optional keyset cursor (S64): `None`/empty means the first page;
/// malformed JSON is `CoreError::InvalidArgument`, never a panic -- the FFI
/// layer maps that straight to `CastellumStatus::InvalidArgument`.
fn parse_cursor(cursor_json: Option<&str>) -> Result<Option<castellum_db::ListCursor>, CoreError> {
    match cursor_json {
        None => Ok(None),
        Some("") => Ok(None),
        Some(s) => serde_json::from_str(s)
            .map(Some)
            .map_err(|_| CoreError::InvalidArgument),
    }
}

/// Parse a sorted-paging request. A cursor that does not belong to the
/// requested sort (a keyed sort without `key`, or a date sort with one) is
/// `InvalidArgument` here, before the db call, because `db_io` would turn a
/// db-level refusal into `CoreError::Db`.
fn parse_sort_and_cursor(
    sort_key: Option<&str>,
    ascending: bool,
    cursor_json: Option<&str>,
) -> Result<(castellum_db::ListSort, Option<castellum_db::ListCursor>), CoreError> {
    let sort =
        castellum_db::ListSort::parse(sort_key, ascending).ok_or(CoreError::InvalidArgument)?;
    let cursor = parse_cursor(cursor_json)?;
    if let Some(c) = &cursor {
        if sort.is_keyed() != c.key.is_some() {
            return Err(CoreError::InvalidArgument);
        }
    }
    Ok((sort, cursor))
}

impl Profile {
    /// All accounts as a JSON array.
    pub fn list_accounts_json(&self) -> Result<String, CoreError> {
        to_json(&self.db.list_accounts().map_err(db_io)?)
    }

    /// Mailboxes for an account as a JSON array.
    pub fn list_mailboxes_json(&self, account_id: &str) -> Result<String, CoreError> {
        to_json(&self.db.list_mailboxes(account_id).map_err(db_io)?)
    }

    /// Per-mailbox total/unread counts for an account as a JSON array.
    pub fn mailbox_counts_json(&self, account_id: &str) -> Result<String, CoreError> {
        to_json(&self.db.mailbox_counts(account_id).map_err(db_io)?)
    }

    /// Per-mailbox body-hydration progress (hydrated / total) for an account
    /// as a JSON array (#3631 G5).
    pub fn body_hydration_progress_json(&self, account_id: &str) -> Result<String, CoreError> {
        to_json(&self.db.body_hydration_progress(account_id).map_err(db_io)?)
    }

    /// Body-hydration coverage of a search scope as `{"total","missing"}`
    /// (#3633 G13). `account_id`/`mailbox_id` narrow when `Some`; both `None`
    /// is every account. `query` (the search text) contributes only its
    /// mailbox filters (`in:`/`folder:`/`path:`). Messages in `missing` have no local body, so a
    /// body match in them cannot be found by search.
    pub fn body_hydration_scope_json(
        &self,
        account_id: Option<&str>,
        mailbox_id: Option<&str>,
        query: Option<&str>,
    ) -> Result<String, CoreError> {
        to_json(
            &self
                .db
                .body_hydration_scope(account_id, mailbox_id, query)
                .map_err(db_io)?,
        )
    }

    // --- write path -------------------------------------------------------

    /// Mark a message read/unread on explicit user intent (applies locally +
    /// enqueues a replay op). Refused in evaluation mode, like every other
    /// explicit write.
    pub fn mark_read(&self, message_id: &str, is_read: bool) -> Result<(), CoreError> {
        self.db.mark_read(message_id, is_read).map_err(db_io)
    }

    /// Mark a message read as a side effect of opening it. Local-only in
    /// evaluation mode instead of refused — reading mail never requires
    /// leaving read-only. See `Database::mark_read_auto`.
    pub fn mark_read_auto(&self, message_id: &str, is_read: bool) -> Result<(), CoreError> {
        self.db.mark_read_auto(message_id, is_read).map_err(db_io)
    }

    /// Mark every unread message in a mailbox read (applies locally + enqueues
    /// one bulk replay op). Returns the number of messages changed.
    pub fn mark_all_read(&self, account_id: &str, mailbox_id: &str) -> Result<usize, CoreError> {
        self.db.mark_all_read(account_id, mailbox_id).map_err(db_io)
    }

    /// Toggle a message's starred (`\Flagged`) state (local + enqueue).
    pub fn set_flagged(&self, message_id: &str, flagged: bool) -> Result<(), CoreError> {
        self.db.set_flagged(message_id, flagged).map_err(db_io)
    }

    /// Archive a message (move to the account's Archive mailbox).
    pub fn archive_message(&self, message_id: &str) -> Result<(), CoreError> {
        self.db.archive_message(message_id).map_err(db_io)
    }

    /// Delete a message (move to the account's Trash mailbox; undoable).
    pub fn delete_message(&self, message_id: &str) -> Result<(), CoreError> {
        self.db.delete_message(message_id).map_err(db_io)
    }

    /// Archive, trash or move a batch of messages (the undoable path).
    /// `request_json` = `{"message_ids":["…"],"target":"archive"|"trash"|"mailbox",
    /// "mailbox_id":"…"}` (`mailbox_id` only for `"mailbox"`). Each message is its
    /// own transaction; one refusal does not stop the rest. Returns
    /// `{"operation_ids":[i64…],"refused":[{"message_id","reason","evaluation_mode"}]}`;
    /// the ids are the undo handles for [`Self::undo_moves_json`].
    pub fn move_messages_json(&self, request_json: &str) -> Result<String, CoreError> {
        #[derive(serde::Deserialize)]
        struct Req {
            message_ids: Vec<String>,
            target: String,
            #[serde(default)]
            mailbox_id: Option<String>,
        }
        let req: Req =
            serde_json::from_str(request_json).map_err(|_| CoreError::InvalidArgument)?;
        let target = match (req.target.as_str(), req.mailbox_id.as_deref()) {
            ("archive", _) => castellum_db::MoveTarget::Archive,
            ("trash", _) => castellum_db::MoveTarget::Trash,
            ("mailbox", Some(id)) if !id.is_empty() => castellum_db::MoveTarget::Mailbox(id),
            _ => return Err(CoreError::InvalidArgument),
        };
        let out = self
            .db
            .move_messages(&req.message_ids, target)
            .map_err(db_io)?;
        to_json(&out)
    }

    /// Queue the inverse of earlier moves. `request_json` = `{"operation_ids":[i64…]}`.
    /// Atomic for the group. Returns `{"queued":n,"account_ids":[…],
    /// "source_mailbox_ids":[…],"not_undoable":[{"operation_id","reason","evaluation_mode"}]}`.
    /// The messages return on the next replay + sync, as new rows.
    pub fn undo_moves_json(&self, request_json: &str) -> Result<String, CoreError> {
        #[derive(serde::Deserialize)]
        struct Req {
            operation_ids: Vec<i64>,
        }
        let req: Req =
            serde_json::from_str(request_json).map_err(|_| CoreError::InvalidArgument)?;
        let out = self.db.undo_moves(&req.operation_ids).map_err(db_io)?;
        to_json(&out)
    }

    /// Permanently delete ONE message that is already in Trash (destructive,
    /// queued through the operation queue; the local row is kept until the
    /// server confirms). Refused for any message outside Trash. The UI must
    /// have shown an explicit confirmation first.
    pub fn delete_message_permanently(&self, message_id: &str) -> Result<(), CoreError> {
        self.db
            .delete_message_permanently(message_id)
            .map_err(db_io)
    }

    /// Permanently empty the account's Trash (destructive).
    pub fn empty_trash(&self, account_id: &str) -> Result<usize, CoreError> {
        self.db.empty_trash(account_id).map_err(db_io)
    }

    /// Pending write-path operations for an account as a JSON array.
    pub fn pending_operations_json(&self, account_id: &str) -> Result<String, CoreError> {
        to_json(&self.db.pending_operations(account_id).map_err(db_io)?)
    }

    /// Manually retry a failed/failed_permanent operation (outbox action).
    /// Returns whether the op was actually reset to `queued`.
    pub fn retry_operation(&self, op_id: i64) -> Result<bool, CoreError> {
        self.db.retry_operation(op_id).map_err(db_io)
    }

    /// Manually cancel an undelivered operation (outbox action). Running ops
    /// are refused. Returns whether the op was cancelled.
    pub fn cancel_operation(&self, op_id: i64) -> Result<bool, CoreError> {
        self.db.cancel_operation(op_id).map_err(db_io)
    }

    // --- drafts (compose) ---------------------------------------------------

    /// Create or update a compose draft from its JSON form; returns the draft id.
    pub fn save_draft_json(&self, draft_json: &str) -> Result<i64, CoreError> {
        let draft: castellum_models::NewDraft = serde_json::from_str(draft_json)
            .map_err(|e| CoreError::Db(format!("bad draft json: {e}")))?;
        self.db.save_draft(&draft).map_err(db_io)
    }

    /// All drafts for an account (any status, newest first) as a JSON array.
    pub fn list_drafts_json(&self, account_id: &str) -> Result<String, CoreError> {
        to_json(&self.db.list_drafts(account_id).map_err(db_io)?)
    }

    /// Delete a draft (refused once it has left `draft` status).
    pub fn delete_draft(&self, draft_id: i64) -> Result<(), CoreError> {
        self.db.delete_draft(draft_id).map_err(db_io)
    }

    /// Build RFC822 bytes for `draft_id` (from `from_json` =
    /// `{"from_email": str, "from_name": str?}`) and enqueue a
    /// `server_draft_save` replay op. `{"queued": bool}` — `false` means the
    /// draft had already left `draft` status (a benign no-op, see
    /// `Database::queue_server_draft_save`).
    pub fn save_draft_to_server_json(
        &self,
        draft_id: i64,
        from_json: &str,
    ) -> Result<String, CoreError> {
        #[derive(serde::Deserialize)]
        struct From {
            from_email: String,
            #[serde(default)]
            from_name: Option<String>,
        }
        let from: From = serde_json::from_str(from_json)
            .map_err(|e| CoreError::Db(format!("bad from json: {e}")))?;
        let Some(draft) = self.db.get_draft(draft_id).map_err(db_io)? else {
            return Err(CoreError::Db(format!("unknown draft id: {draft_id}")));
        };
        if draft.status != "draft" {
            return to_json(&serde_json::json!({ "queued": false }));
        }
        let built = castellum_provider_smtp::build_draft_rfc822(
            &from.from_email,
            from.from_name.as_deref(),
            &draft,
        )
        .map_err(|e| CoreError::Sync(e.to_string()))?;
        self.db
            .queue_server_draft_save(draft_id, &built.message_id, &built.raw)
            .map_err(db_io)?;
        to_json(&serde_json::json!({ "queued": true }))
    }

    /// Hydrate (or reuse) a local editable draft from an already-synced
    /// message in a Drafts-role mailbox. Returns the draft id.
    ///
    /// Attachments already in the on-disk attachment cache are copied into
    /// the draft (#3436). No network: an attachment that was never
    /// downloaded makes the hydration incomplete, and `Database::
    /// open_server_draft_with` then leaves the draft untracked so the
    /// original server copy is never expunged on its behalf.
    pub fn open_server_draft_json(&self, message_id: &str) -> Result<i64, CoreError> {
        let profile_path = self.path();
        self.db
            .open_server_draft_with(message_id, |content_hash| {
                std::fs::read(crate::attachments::cached_file_path(
                    profile_path,
                    content_hash,
                ))
                .ok()
            })
            .map_err(db_io)
    }

    /// Queue a draft for delivery: status -> `outbox` + one `send` op, atomically.
    pub fn send_draft(&self, draft_id: i64) -> Result<(), CoreError> {
        self.db.send_draft(draft_id).map_err(db_io)
    }

    /// Queue a draft for delivery with an undo-send window (S25):
    /// `delay_seconds <= 0` behaves exactly like `send_draft`. See
    /// `castellum_db::Database::send_draft_delayed`.
    pub fn send_draft_delayed(&self, draft_id: i64, delay_seconds: i64) -> Result<(), CoreError> {
        self.db
            .send_draft_delayed(draft_id, delay_seconds)
            .map_err(db_io)
    }

    /// Undo a not-yet-submitted queued send. `{"in_time": bool}` — `false`
    /// means the sender already claimed it (or there was nothing to undo).
    pub fn undo_send_json(&self, draft_id: i64) -> Result<String, CoreError> {
        let in_time = self.db.undo_send(draft_id).map_err(db_io)?;
        to_json(&serde_json::json!({ "in_time": in_time }))
    }

    /// Send Later (HP1): queue a draft for delivery at `send_at` (Unix epoch
    /// seconds, UTC). Refused when `send_at` is not in the future. See
    /// `castellum_db::Database::schedule_send`.
    pub fn schedule_send(&self, draft_id: i64, send_at: i64) -> Result<(), CoreError> {
        if send_at <= crate::replay::now_epoch() {
            return Err(CoreError::InvalidArgument);
        }
        self.db.schedule_send(draft_id, send_at).map_err(db_io)
    }

    /// Move a waiting scheduled send to `send_at`. `{"rescheduled": bool}`.
    pub fn reschedule_send_json(&self, draft_id: i64, send_at: i64) -> Result<String, CoreError> {
        if send_at <= crate::replay::now_epoch() {
            return Err(CoreError::InvalidArgument);
        }
        let moved = self.db.reschedule_send(draft_id, send_at).map_err(db_io)?;
        to_json(&serde_json::json!({ "rescheduled": moved }))
    }

    /// Cancel a scheduled send and return its draft, intact, to Drafts.
    /// `{"cancelled": bool}` (`false` = already claimed by the sender).
    pub fn cancel_scheduled_send_json(&self, draft_id: i64) -> Result<String, CoreError> {
        let cancelled = self.db.undo_send(draft_id).map_err(db_io)?;
        to_json(&serde_json::json!({ "cancelled": cancelled }))
    }

    /// Release a `held` send (crash-safety hold) for immediate delivery on
    /// the next drain. `{"released": bool}`.
    pub fn send_now_json(&self, draft_id: i64) -> Result<String, CoreError> {
        let released = self.db.send_now(draft_id).map_err(db_io)?;
        to_json(&serde_json::json!({ "released": released }))
    }

    /// OB1: every message waiting in the Outbox for `account_id` (queued, in
    /// flight, failed or stranded) as a JSON array of `OutboxSend`. Carries
    /// failure kinds only, never server text.
    pub fn outbox_sends_json(&self, account_id: &str) -> Result<String, CoreError> {
        to_json(&crate::send::outbox_sends(
            &self.db,
            account_id,
            crate::replay::now_epoch(),
        )?)
    }

    /// OB1 Outbox "Send Now": attempt exactly `draft_id` over SMTP now
    /// (`config_json` is a `SendConfig`). JSON `SendReport` for that message.
    pub fn send_draft_now_json(
        &self,
        config_json: &str,
        draft_id: i64,
    ) -> Result<String, CoreError> {
        crate::send::send_draft_now_json(&self.db, config_json, draft_id)
    }

    /// OB1 Outbox "Send All": give every failed send of `account_id` a fresh
    /// attempt (permanent failures included) before the caller drains.
    /// `{"requeued": n}`.
    pub fn requeue_failed_sends_json(&self, account_id: &str) -> Result<String, CoreError> {
        let requeued = self.db.requeue_failed_sends(account_id).map_err(db_io)?;
        to_json(&serde_json::json!({ "requeued": requeued }))
    }

    /// Crash-safety startup pass (S25 — see DECISIONS.md). Call once per
    /// account at launch, before scheduling any drain. `{"held": n}`.
    pub fn reconcile_undo_sends_json(&self, account_id: &str) -> Result<String, CoreError> {
        let held = self.db.reconcile_undo_sends(account_id).map_err(db_io)?;
        to_json(&serde_json::json!({ "held": held }))
    }

    /// Remember (or update) whether `from_address` may load remote content
    /// automatically for `account_id` (S48). Plain (non-JSON) passthrough for
    /// the FFI's `profile_action` helper, which expects `Result<(), _>`.
    pub fn set_sender_remote_content_allowed(
        &self,
        account_id: &str,
        from_address: &str,
        allowed: bool,
    ) -> Result<(), CoreError> {
        self.db
            .set_sender_remote_content_allowed(account_id, from_address, allowed)
            .map_err(db_io)
    }

    /// Forget a remembered sender policy. Plain (non-JSON) passthrough for
    /// the FFI's `profile_action` helper.
    pub fn revoke_sender_remote_content(
        &self,
        account_id: &str,
        from_address: &str,
    ) -> Result<(), CoreError> {
        self.db
            .revoke_sender_remote_content(account_id, from_address)
            .map_err(db_io)
    }

    /// Remember (or update) whether `from_address` may load remote content
    /// automatically for `account_id` (S48). `{"ok": true}` on success.
    pub fn set_sender_remote_content_allowed_json(
        &self,
        account_id: &str,
        from_address: &str,
        allowed: bool,
    ) -> Result<String, CoreError> {
        self.db
            .set_sender_remote_content_allowed(account_id, from_address, allowed)
            .map_err(db_io)?;
        to_json(&serde_json::json!({ "ok": true }))
    }

    /// `null` JSON if no remembered policy, else the boolean decision.
    pub fn sender_remote_content_allowed_json(
        &self,
        account_id: &str,
        from_address: &str,
    ) -> Result<String, CoreError> {
        to_json(
            &self
                .db
                .sender_remote_content_allowed(account_id, from_address)
                .map_err(db_io)?,
        )
    }

    /// Every remembered sender policy across every account, for the Settings
    /// review list.
    pub fn list_sender_remote_content_policies_json(&self) -> Result<String, CoreError> {
        to_json(
            &self
                .db
                .list_sender_remote_content_policies()
                .map_err(db_io)?,
        )
    }

    /// Forget a remembered sender policy. `{"ok": true}` even if none existed.
    pub fn revoke_sender_remote_content_json(
        &self,
        account_id: &str,
        from_address: &str,
    ) -> Result<String, CoreError> {
        self.db
            .revoke_sender_remote_content(account_id, from_address)
            .map_err(db_io)?;
        to_json(&serde_json::json!({ "ok": true }))
    }

    /// Message headers for a mailbox (newest first, paginated) as a JSON array.
    pub fn list_messages_json(
        &self,
        account_id: &str,
        mailbox_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<String, CoreError> {
        // Clamp so a negative limit doesn't become SQLite's "no limit" (return all).
        let (limit, offset) = (limit.max(0), offset.max(0));
        to_json(
            &self
                .db
                .list_message_headers(account_id, mailbox_id, limit, offset)
                .map_err(db_io)?,
        )
    }

    /// Keyset (cursor) page of message headers for a mailbox, newest first
    /// (S64). `cursor_json` is `null`/absent for the first page, or the
    /// `next` object from the previous page's `MessagePage`. Same row shape,
    /// ordering and snooze exclusion as `list_messages_json`; cost is
    /// independent of how deep the page is.
    pub fn list_messages_page_json(
        &self,
        account_id: &str,
        mailbox_id: &str,
        cursor_json: Option<&str>,
        limit: i64,
    ) -> Result<String, CoreError> {
        let limit = limit.max(0);
        let cursor = parse_cursor(cursor_json)?;
        to_json(
            &self
                .db
                .list_message_page(account_id, mailbox_id, cursor.as_ref(), limit)
                .map_err(db_io)?,
        )
    }

    /// Same shape as [`Self::list_messages_page_json`], scoped by mailbox
    /// role across every account (the unified-inbox list).
    pub fn list_messages_by_role_page_json(
        &self,
        role: &str,
        cursor_json: Option<&str>,
        limit: i64,
    ) -> Result<String, CoreError> {
        let limit = limit.max(0);
        let cursor = parse_cursor(cursor_json)?;
        to_json(
            &self
                .db
                .list_role_page(role, cursor.as_ref(), limit)
                .map_err(db_io)?,
        )
    }

    /// [`Self::list_messages_page_json`] in a chosen order. `sort_key` is
    /// `"date"`, `"from"` or `"subject"` (`None`/`""` = date); anything else is
    /// `InvalidArgument`. `cursor_json` must come from a page of the SAME sort
    /// and direction: a text sort needs a cursor carrying `key`, a date sort
    /// must not have one.
    pub fn list_messages_page_sorted_json(
        &self,
        account_id: &str,
        mailbox_id: &str,
        sort_key: Option<&str>,
        ascending: bool,
        cursor_json: Option<&str>,
        limit: i64,
    ) -> Result<String, CoreError> {
        let (sort, cursor) = parse_sort_and_cursor(sort_key, ascending, cursor_json)?;
        to_json(
            &self
                .db
                .list_message_page_sorted(
                    account_id,
                    mailbox_id,
                    sort,
                    cursor.as_ref(),
                    limit.max(0),
                )
                .map_err(db_io)?,
        )
    }

    /// [`Self::list_messages_by_role_page_json`] in a chosen order; see
    /// [`Self::list_messages_page_sorted_json`].
    pub fn list_messages_by_role_page_sorted_json(
        &self,
        role: &str,
        sort_key: Option<&str>,
        ascending: bool,
        cursor_json: Option<&str>,
        limit: i64,
    ) -> Result<String, CoreError> {
        let (sort, cursor) = parse_sort_and_cursor(sort_key, ascending, cursor_json)?;
        to_json(
            &self
                .db
                .list_role_page_sorted(role, sort, cursor.as_ref(), limit.max(0))
                .map_err(db_io)?,
        )
    }

    /// A full message by id as JSON (`null` if not found).
    pub fn get_message_json(&self, message_id: &str) -> Result<String, CoreError> {
        to_json(&self.db.get_message_detail(message_id).map_err(db_io)?)
    }

    /// Search within an account; JSON array of headers, ranked. `query`
    /// accepts the S39 search grammar (`from:`/`to:`/`cc:`/`subject:`/
    /// `has:attachment`/`before:`/`after:`/`in:`/`is:read|unread|flagged|
    /// starred`, `-` negation, quoted phrases) as well as bare keywords —
    /// see `castellum_db::search_query`.
    pub fn search_messages_json(
        &self,
        account_id: &str,
        query: &str,
        limit: i64,
    ) -> Result<String, CoreError> {
        let limit = limit.max(0);
        to_json(
            &self
                .db
                .search_messages(account_id, query, limit)
                .map_err(db_io)?,
        )
    }

    /// Thread summaries for an account (newest first, paginated) as JSON.
    pub fn list_threads_json(
        &self,
        account_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<String, CoreError> {
        let (limit, offset) = (limit.max(0), offset.max(0));
        to_json(
            &self
                .db
                .list_thread_summaries(account_id, limit, offset)
                .map_err(db_io)?,
        )
    }

    /// All message headers in a thread (oldest first) as JSON.
    pub fn get_thread_json(&self, thread_id: &str) -> Result<String, CoreError> {
        to_json(&self.db.get_thread_messages(thread_id).map_err(db_io)?)
    }

    // --- unified (cross-account) reads (S34) -------------------------------

    /// Message headers across every account whose mailbox has `role` (newest
    /// first, paginated) as a JSON array — the core half of "All Inboxes".
    pub fn list_messages_by_role_json(
        &self,
        role: &str,
        limit: i64,
        offset: i64,
    ) -> Result<String, CoreError> {
        let (limit, offset) = (limit.max(0), offset.max(0));
        to_json(
            &self
                .db
                .list_messages_by_role(role, limit, offset)
                .map_err(db_io)?,
        )
    }

    /// Every thread across every account (newest activity first, paginated)
    /// as a JSON array. Not yet wired to any view — see `list_threads_json`.
    pub fn list_threads_across_accounts_json(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<String, CoreError> {
        let (limit, offset) = (limit.max(0), offset.max(0));
        to_json(
            &self
                .db
                .list_thread_summaries_across_accounts(limit, offset)
                .map_err(db_io)?,
        )
    }

    /// Search across every account; JSON array of headers, ranked. Same S39
    /// grammar as `search_messages_json`.
    pub fn search_messages_across_accounts_json(
        &self,
        query: &str,
        limit: i64,
    ) -> Result<String, CoreError> {
        let limit = limit.max(0);
        to_json(
            &self
                .db
                .search_messages_across_accounts(query, limit)
                .map_err(db_io)?,
        )
    }

    // --- snooze (S43) --------------------------------------------------------

    /// Snooze `message_id` until `until_epoch` (Unix epoch seconds).
    pub fn snooze_message(&self, message_id: &str, until_epoch: i64) -> Result<(), CoreError> {
        self.db
            .snooze_message(message_id, until_epoch)
            .map_err(db_io)
    }

    /// Un-snooze `message_id` now. `{"unsnoozed": bool}`.
    pub fn unsnooze_message_json(&self, message_id: &str) -> Result<String, CoreError> {
        let unsnoozed = self.db.unsnooze_message(message_id).map_err(db_io)?;
        to_json(&serde_json::json!({ "unsnoozed": unsnoozed }))
    }

    /// Every currently-snoozed message, soonest due first, as a JSON array —
    /// the Snoozed sidebar list.
    pub fn list_snoozed_messages_json(&self, limit: i64, offset: i64) -> Result<String, CoreError> {
        let (limit, offset) = (limit.max(0), offset.max(0));
        to_json(
            &self
                .db
                .list_snoozed_messages(limit, offset)
                .map_err(db_io)?,
        )
    }

    /// Pop every snooze due at or before `now_epoch`. `[{"message_id":...,
    /// "account_id":...}, ...]` — call on launch and on a periodic sweep.
    pub fn unsnooze_due_json(&self, now_epoch: i64) -> Result<String, CoreError> {
        to_json(&self.db.unsnooze_due(now_epoch).map_err(db_io)?)
    }

    // --- tags and mute (S42) -------------------------------------------------

    /// Every tag definition as a JSON array, in display order.
    pub fn list_tags_json(&self) -> Result<String, CoreError> {
        to_json(&self.db.list_tag_definitions().map_err(db_io)?)
    }

    /// Create (empty `keyword`) or update a tag definition from a JSON
    /// `TagDefinition`; returns the stored row (with `keyword`/`keyword_key`
    /// filled in on create).
    pub fn upsert_tag_json(&self, tag_json: &str) -> Result<String, CoreError> {
        let def: castellum_models::TagDefinition =
            serde_json::from_str(tag_json).map_err(|_| CoreError::Io)?;
        to_json(&self.db.upsert_tag_definition(&def).map_err(db_io)?)
    }

    /// Delete a tag definition (the definition only — the keyword itself
    /// stays on the server and on any message that already carries it).
    pub fn delete_tag(&self, keyword_key: &str) -> Result<(), CoreError> {
        self.db.delete_tag_definition(keyword_key).map_err(db_io)?;
        Ok(())
    }

    /// Add or remove one IMAP keyword on a message. `{"synced": bool,
    /// "reason": null|"evaluation_mode"|"not_permanent"}`.
    pub fn set_message_tag_json(
        &self,
        message_id: &str,
        keyword: &str,
        add: bool,
    ) -> Result<String, CoreError> {
        to_json(
            &self
                .db
                .set_message_keyword(message_id, keyword, add)
                .map_err(db_io)?,
        )
    }

    /// Mute/unmute `message_id`'s whole conversation (local only).
    pub fn set_thread_muted(&self, message_id: &str, muted: bool) -> Result<(), CoreError> {
        self.db.mute_thread(message_id, muted).map_err(db_io)
    }

    /// Every distinct visible keyword in use with a message count — the
    /// Settings "In use on your servers" adopt list.
    pub fn list_keywords_in_use_json(&self) -> Result<String, CoreError> {
        to_json(&self.db.list_keywords_in_use().map_err(db_io)?)
    }

    /// `[{keyword_key, total, unread, local_only}]` for every defined tag.
    pub fn tag_counts_json(&self) -> Result<String, CoreError> {
        to_json(&self.db.tag_counts().map_err(db_io)?)
    }

    /// MessageHeader array for one defined tag, newest first, keyset-paged (MM-8).
    pub fn messages_for_tag_json(
        &self,
        keyword: &str,
        limit: i64,
        after: Option<(i64, &str)>,
    ) -> Result<String, CoreError> {
        to_json(
            &self
                .db
                .messages_for_tag(keyword, limit.max(0), after)
                .map_err(db_io)?,
        )
    }

    // --- VIP senders and the notification outbox (S59) ---------------------

    /// Add/remove a VIP; `{"changed": bool}`. `display_name` is optional.
    pub fn set_vip_json(
        &self,
        address: &str,
        display_name: Option<&str>,
        vip: bool,
    ) -> Result<String, CoreError> {
        let changed = self
            .db
            .set_vip(address, display_name, vip, wall_clock_secs())
            .map_err(db_io)?;
        to_json(&serde_json::json!({ "changed": changed }))
    }

    /// Every VIP as a JSON array of `VipSender`.
    pub fn list_vips_json(&self) -> Result<String, CoreError> {
        to_json(&self.db.list_vips().map_err(db_io)?)
    }

    /// Pop up to `limit` undelivered new-mail notices (at-most-once) as a
    /// JSON array of `PendingNotification`.
    pub fn take_pending_notifications_json(&self, limit: i64) -> Result<String, CoreError> {
        to_json(
            &self
                .db
                .take_pending_notifications(limit.clamp(1, 500), wall_clock_secs())
                .map_err(db_io)?,
        )
    }

    /// Unread inbox mail from VIPs, for the given accounts (`None` = all).
    pub fn vip_unread_count(&self, account_ids: Option<Vec<String>>) -> Result<i64, CoreError> {
        self.db
            .vip_unread_count(account_ids.as_deref())
            .map_err(db_io)
    }

    // --- account reset --------------------------------

    /// What an unforced `reset_account` would refuse on, as JSON — the
    /// pre-check the confirmation dialog reads.
    pub fn reset_account_blockers_json(&self, account_id: &str) -> Result<String, CoreError> {
        to_json(&self.db.reset_account_blockers(account_id).map_err(db_io)?)
    }

    /// Wipe an account's local mail state (see `Database::reset_account`).
    /// `force = false` refuses while an outbox draft or queued operation is
    /// still un-replayed; `force = true` discards them too.
    pub fn reset_account(&self, account_id: &str, force: bool) -> Result<(), CoreError> {
        let orphaned = self.db.reset_account(account_id, force).map_err(db_io)?;
        // Downloaded attachment bytes: the rows cascaded away in the
        // transaction, so the LRU evictor can never see these files again.
        // Best-effort unlink AFTER commit (a leftover file is only wasted
        // disk, never a dangling row).
        for hash in orphaned {
            let _ = std::fs::remove_file(crate::attachments::cached_file_path(self.path(), &hash));
        }
        // Raw-source `.eml` files of the wiped messages (#3364).
        let _ = self.sweep_orphaned_caches(crate::ORPHAN_SWEEP_MIN_AGE);
        Ok(())
    }

    // --- folder-selection edit for an existing account -----

    /// What an unforced `apply_mailbox_selection_json` would refuse on, as a
    /// JSON array of `{"path": ..., "pending_operations": n}` — the
    /// confirmation dialog's wording source, exactly like
    /// `reset_account_blockers_json` is for `reset_account`. See
    /// `Database::apply_mailbox_selection_blockers`.
    pub fn apply_mailbox_selection_blockers_json(
        &self,
        account_id: &str,
        keep_paths_json: &str,
    ) -> Result<String, CoreError> {
        let keep_paths: Vec<String> =
            serde_json::from_str(keep_paths_json).map_err(|_| CoreError::InvalidArgument)?;
        to_json(
            &self
                .db
                .apply_mailbox_selection_blockers(account_id, &keep_paths)
                .map_err(db_io)?,
        )
    }

    /// Apply an edited folder selection to an ALREADY-ADDED account (see
    /// `Database::apply_mailbox_selection`) — Settings > Accounts > Folders…,
    /// never the S62 first-sync picker. `keep_paths_json` is a JSON array of
    /// the paths the user still wants synced and must not be empty (rejected
    /// as `CoreError::InvalidArgument`, same as malformed JSON, before ever
    /// reaching the database — belt-and-braces alongside the DB layer's own
    /// guard: emptying the selection would remove every folder). Every other
    /// locally known mailbox for the account is removed immediately UNLESS a
    /// non-terminal operation still touches it, in which case the WHOLE call
    /// is refused (see `Database::apply_mailbox_selection`'s doc comment) —
    /// call `apply_mailbox_selection_blockers_json` first to word that
    /// refusal, then retry with `force = true` only on explicit user
    /// confirmation. Returns `{"removed": n}`. Never touches the server or
    /// credentials, and never triggers a sync.
    ///
    /// Exactly like `reset_account`: downloaded attachment bytes whose only
    /// referencing rows just cascaded away are unlinked from disk AFTER the
    /// DB change, best-effort — a leftover file on failure is only wasted
    /// disk, never a dangling row.
    pub fn apply_mailbox_selection_json(
        &self,
        account_id: &str,
        keep_paths_json: &str,
        force: bool,
    ) -> Result<String, CoreError> {
        let keep_paths: Vec<String> =
            serde_json::from_str(keep_paths_json).map_err(|_| CoreError::InvalidArgument)?;
        if keep_paths.is_empty() {
            return Err(CoreError::InvalidArgument);
        }
        let (removed, orphaned) = self
            .db
            .apply_mailbox_selection(account_id, &keep_paths, force)
            .map_err(db_io)?;
        for hash in orphaned {
            let _ = std::fs::remove_file(crate::attachments::cached_file_path(self.path(), &hash));
        }
        let _ = self.sweep_orphaned_caches(crate::ORPHAN_SWEEP_MIN_AGE);
        to_json(&serde_json::json!({ "removed": removed }))
    }
}

#[cfg(test)]
mod tests {
    use crate::{open_profile, CoreError};
    use castellum_models::{Account, Mailbox};

    fn profile(tag: &str) -> crate::Profile {
        let dir = castellum_testutil::scratch_root().join(format!(
            "castellum-query-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        open_profile(&dir).unwrap()
    }

    /// One account with a Drafts-role mailbox holding a draft another client
    /// wrote (uid 9), carrying one attachment descriptor.
    fn seed_server_draft_with_attachment(p: &crate::Profile) {
        use castellum_models::{AttachmentDescriptor, NewMessage};
        p.db.upsert_account(&Account {
            id: "a".into(),
            email: "a@x".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        p.db.upsert_mailbox(&Mailbox {
            id: "a:Drafts".into(),
            account_id: "a".into(),
            path: "Drafts".into(),
            name: "Drafts".into(),
            role: Some("drafts".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        p.db.upsert_message(&NewMessage {
            id: "a:Drafts:9".into(),
            account_id: "a".into(),
            mailbox_id: "a:Drafts".into(),
            provider_uid: Some(9),
            rfc822_message_id: Some("<webmail@x>".into()),
            subject: Some("Re: plan".into()),
            from_name: None,
            from_address: Some("a@x".into()),
            sent_at: Some(1000),
            snippet: None,
            size_bytes: None,
            has_attachments: true,
            attachments: Some(vec![AttachmentDescriptor {
                ordinal: 0,
                filename: Some("report.pdf".into()),
                content_type: Some("application/pdf".into()),
                size_bytes: Some(11),
                section: "2".into(),
                content_id: None,
                is_inline: false,
                transfer_encoding: Some("base64".into()),
            }]),
            is_read: true,
            is_starred: false,
            addresses: vec![],
            text_body: Some("see attached".into()),
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: vec!["parent@x".into()],
        })
        .unwrap();
    }

    // #3436: a draft written elsewhere keeps its cached attachment when it
    // is opened for editing, read from the real on-disk cache.
    #[test]
    fn open_server_draft_carries_a_cached_attachment_from_disk() {
        use crate::attachments::{cache_dir, cached_file_path};
        let p = profile("osd-cached");
        seed_server_draft_with_attachment(&p);
        std::fs::create_dir_all(cache_dir(p.path())).unwrap();
        std::fs::write(cached_file_path(p.path(), "h1"), b"hello world").unwrap();
        p.db.upsert_attachment_cache_entry("a:Drafts:9", 0, "h1", 11, 1)
            .unwrap();

        let id = p.open_server_draft_json("a:Drafts:9").unwrap();
        let atts = p.db.list_draft_attachments(id).unwrap();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].filename, "report.pdf");
        assert_eq!(
            p.db.draft_attachment_bytes(atts[0].id).unwrap().unwrap(),
            b"hello world"
        );
        let draft = p.db.get_draft(id).unwrap().unwrap();
        assert_eq!(draft.server_uid, Some(9));
        assert!(draft.from_email.is_none(), "own address = the NULL default");
        assert_eq!(draft.in_reply_to.as_deref(), Some("<parent@x>"));
    }

    // #3436: when the attachment was never downloaded the draft is left
    // untracked, so no save, discard or send can expunge the original.
    #[test]
    fn open_server_draft_without_the_cached_file_leaves_the_original_untracked() {
        let p = profile("osd-uncached");
        seed_server_draft_with_attachment(&p);
        // A cache ROW whose file is gone counts as unavailable too.
        p.db.upsert_attachment_cache_entry("a:Drafts:9", 0, "missing", 11, 1)
            .unwrap();

        let id = p.open_server_draft_json("a:Drafts:9").unwrap();
        assert!(p.db.list_draft_attachments(id).unwrap().is_empty());
        let draft = p.db.get_draft(id).unwrap().unwrap();
        assert!(draft.server_uid.is_none(), "{draft:?}");
        assert_eq!(p.open_server_draft_json("a:Drafts:9").unwrap(), id);
    }

    #[test]
    fn reset_account_deletes_cached_attachment_files_but_keeps_shared_ones() {
        use crate::attachments::{cache_dir, cached_file_path};
        use castellum_models::NewMessage;
        let p = profile("reset-att");
        for id in ["a", "b"] {
            p.db.upsert_account(&Account {
                id: id.into(),
                email: format!("{id}@x"),
                display_name: None,
                provider: "imap".into(),
            })
            .unwrap();
            p.db.upsert_mailbox(&Mailbox {
                id: format!("{id}:INBOX"),
                account_id: id.into(),
                path: "INBOX".into(),
                name: "INBOX".into(),
                role: Some("inbox".into()),
                delimiter: Some("/".into()),
                subscribed: true,
                selectable: true,
                sync_by_default: true,
            })
            .unwrap();
            p.db.upsert_message(&NewMessage {
                id: format!("{id}:m"),
                account_id: id.into(),
                mailbox_id: format!("{id}:INBOX"),
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
            })
            .unwrap();
        }
        std::fs::create_dir_all(cache_dir(p.path())).unwrap();
        for h in ["own", "shared"] {
            std::fs::write(cached_file_path(p.path(), h), b"bytes").unwrap();
        }
        p.db.upsert_attachment_cache_entry("a:m", 0, "own", 5, 1)
            .unwrap();
        p.db.upsert_attachment_cache_entry("a:m", 1, "shared", 5, 1)
            .unwrap();
        p.db.upsert_attachment_cache_entry("b:m", 0, "shared", 5, 1)
            .unwrap();

        p.reset_account("a", false).unwrap();

        assert!(
            !cached_file_path(p.path(), "own").exists(),
            "orphaned bytes removed"
        );
        assert!(
            cached_file_path(p.path(), "shared").exists(),
            "shared bytes kept"
        );
        assert_eq!(p.db.total_attachment_cache_bytes().unwrap(), 5);
    }

    #[test]
    fn query_apis_emit_json() {
        let p = profile("q");
        // Seed via the db directly (sync is covered elsewhere).
        p.db.upsert_account(&Account {
            id: "a".into(),
            email: "e@x".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        p.db.upsert_mailbox(&Mailbox {
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

        let accounts = p.list_accounts_json().unwrap();
        assert!(accounts.contains("\"email\":\"e@x\""));
        let mailboxes = p.list_mailboxes_json("a").unwrap();
        assert!(mailboxes.contains("\"path\":\"INBOX\""));
        // Empty results serialize as an empty array; missing detail as null.
        assert_eq!(p.list_messages_json("a", "a:INBOX", 50, 0).unwrap(), "[]");
        assert_eq!(p.get_message_json("nope").unwrap(), "null");
        assert_eq!(p.search_messages_json("a", "anything", 10).unwrap(), "[]");
        // S34: the unified (cross-account) counterparts emit the same shapes.
        assert_eq!(p.list_messages_by_role_json("inbox", 50, 0).unwrap(), "[]");
        assert_eq!(p.list_threads_across_accounts_json(50, 0).unwrap(), "[]");
        assert_eq!(
            p.search_messages_across_accounts_json("anything", 10)
                .unwrap(),
            "[]"
        );
        // S35: an unknown account has no blockers and a reset on it errors.
        assert_eq!(
            p.reset_account_blockers_json("a").unwrap(),
            "{\"unsent_outbox_drafts\":0,\"pending_operations\":0}"
        );
        assert!(p.reset_account("nope", false).is_err());
    }

    #[test]
    fn apply_mailbox_selection_json_prunes_and_reports_the_removed_count() {
        let p = profile("scope");
        p.db.upsert_account(&Account {
            id: "a".into(),
            email: "e@x".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        for path in ["INBOX", "Old"] {
            p.db.upsert_mailbox(&Mailbox {
                id: format!("a:{path}"),
                account_id: "a".into(),
                path: path.into(),
                name: path.into(),
                role: None,
                delimiter: Some("/".into()),
                subscribed: true,
                selectable: true,
                sync_by_default: path == "INBOX",
            })
            .unwrap();
        }

        assert_eq!(
            p.apply_mailbox_selection_json("a", "[\"INBOX\"]", false)
                .unwrap(),
            "{\"removed\":1}"
        );
        let remaining = p.list_mailboxes_json("a").unwrap();
        assert!(remaining.contains("\"path\":\"INBOX\""));
        assert!(!remaining.contains("\"path\":\"Old\""));

        // Malformed keep_paths JSON, and an empty keep list, are both a typed
        // InvalidArgument — never a DB call, never CoreError::Sync.
        assert!(matches!(
            p.apply_mailbox_selection_json("a", "not json", false),
            Err(CoreError::InvalidArgument)
        ));
        assert!(matches!(
            p.apply_mailbox_selection_json("a", "[]", false),
            Err(CoreError::InvalidArgument)
        ));
        assert!(matches!(
            p.apply_mailbox_selection_blockers_json("a", "not json"),
            Err(CoreError::InvalidArgument)
        ));
    }

    #[test]
    fn apply_mailbox_selection_json_blocks_unforced_and_force_cancels_with_a_reason() {
        let p = profile("scope-block");
        p.db.upsert_account(&Account {
            id: "a".into(),
            email: "e@x".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        for path in ["INBOX", "Old"] {
            p.db.upsert_mailbox(&Mailbox {
                id: format!("a:{path}"),
                account_id: "a".into(),
                path: path.into(),
                name: path.into(),
                role: None,
                delimiter: Some("/".into()),
                subscribed: true,
                selectable: true,
                sync_by_default: path == "INBOX",
            })
            .unwrap();
        }
        p.db.set_account_write_enabled("a", true).unwrap();
        use castellum_models::NewMessage;
        p.db.upsert_message(&NewMessage {
            id: "a:Old:1".into(),
            account_id: "a".into(),
            mailbox_id: "a:Old".into(),
            provider_uid: Some(1),
            rfc822_message_id: Some("<o1@x>".into()),
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
        })
        .unwrap();
        p.db.mark_read("a:Old:1", true).unwrap();

        let blockers = p
            .apply_mailbox_selection_blockers_json("a", "[\"INBOX\"]")
            .unwrap();
        assert!(blockers.contains("\"path\":\"Old\""));
        assert!(blockers.contains("\"pending_operations\":1"));
        assert!(p
            .apply_mailbox_selection_json("a", "[\"INBOX\"]", false)
            .is_err());
        assert!(p
            .list_mailboxes_json("a")
            .unwrap()
            .contains("\"path\":\"Old\""));

        assert_eq!(
            p.apply_mailbox_selection_json("a", "[\"INBOX\"]", true)
                .unwrap(),
            "{\"removed\":1}"
        );
        assert!(!p
            .list_mailboxes_json("a")
            .unwrap()
            .contains("\"path\":\"Old\""));
    }

    #[test]
    fn save_draft_json_round_trips_body_html() {
        let p = profile("draft-md");
        p.db.upsert_account(&Account {
            id: "a".into(),
            email: "e@x".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();

        let with_html = serde_json::json!({
            "account_id": "a",
            "to": [{"email": "bob@example.com"}],
            "body_text": "**hi**",
            "body_html": "<p><strong>hi</strong></p>"
        })
        .to_string();
        p.save_draft_json(&with_html).unwrap();
        assert!(p
            .list_drafts_json("a")
            .unwrap()
            .contains("<p><strong>hi</strong></p>"));

        let without_html = serde_json::json!({
            "account_id": "a",
            "to": [{"email": "carol@example.com"}],
            "body_text": "plain only"
        })
        .to_string();
        let id2 = p.save_draft_json(&without_html).unwrap();
        let list: serde_json::Value =
            serde_json::from_str(&p.list_drafts_json("a").unwrap()).unwrap();
        let second = list
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == id2)
            .unwrap();
        assert!(
            second["body_html"].is_null(),
            "Markdown-off drafts serialize body_html as null"
        );
    }

    #[test]
    fn search_query_grammar_reaches_the_json_layer_without_erroring() {
        let p = profile("search-grammar");
        for q in [
            "from:alice to:bob cc:carol subject:\"weekly report\"",
            "has:attachment before:2026-09-18 after:2026-01-01",
            "in:inbox is:unread -is:flagged",
            "-spam \"quoted phrase\" bare",
            "",
            "   ",
            "-",
            "::::",
            "\"unterminated",
        ] {
            assert_eq!(p.search_messages_json("a", q, 10).unwrap(), "[]");
            assert_eq!(p.search_messages_across_accounts_json(q, 10).unwrap(), "[]");
        }
    }

    /// #3364: removing an account unlinks its downloaded attachments and
    /// cached raw sources, keeps a hash another account still references,
    /// and the sweep removes orphans no per-path unlink could name.
    #[test]
    fn delete_account_removes_cached_bytes_and_the_sweep_removes_orphans() {
        use castellum_models::NewMessage;
        let p = profile("delete-account-caches");
        let mk = |acc: &str| {
            p.db.upsert_account(&Account {
                id: acc.into(),
                email: format!("{acc}@x.test"),
                display_name: None,
                provider: "imap".into(),
            })
            .unwrap();
            p.db.upsert_mailbox(&Mailbox {
                id: format!("{acc}:INBOX"),
                account_id: acc.into(),
                path: "INBOX".into(),
                name: "INBOX".into(),
                role: Some("inbox".into()),
                delimiter: Some("/".into()),
                subscribed: true,
                selectable: true,
                sync_by_default: true,
            })
            .unwrap();
            p.db.upsert_message(&NewMessage {
                id: format!("{acc}:INBOX:1"),
                account_id: acc.into(),
                mailbox_id: format!("{acc}:INBOX"),
                provider_uid: Some(1),
                rfc822_message_id: None,
                subject: None,
                from_name: None,
                from_address: None,
                sent_at: Some(1),
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
            })
            .unwrap();
        };
        mk("a");
        mk("b");
        let att = crate::attachments::cache_dir(p.path());
        let raw = crate::raw_source::cache_dir(p.path());
        std::fs::create_dir_all(&att).unwrap();
        std::fs::create_dir_all(&raw).unwrap();
        // a: one private hash, one shared with b. b: the shared one.
        for (msg, ord, hash) in [
            ("a:INBOX:1", 0, "aa"),
            ("a:INBOX:1", 1, "shared"),
            ("b:INBOX:1", 0, "shared"),
        ] {
            p.db.upsert_attachment_cache_entry(msg, ord, hash, 3, 1)
                .unwrap();
            std::fs::write(att.join(hash), b"xyz").unwrap();
        }
        for id in ["a:INBOX:1", "b:INBOX:1"] {
            std::fs::write(crate::raw_source::cached_file_path(p.path(), id), b"eml").unwrap();
        }
        // An orphan nothing names (e.g. from an expunged message).
        std::fs::write(att.join("orphan"), b"x").unwrap();
        std::fs::write(raw.join("deadbeef.eml"), b"x").unwrap();

        p.delete_account("a").unwrap();
        // The default sweep spares fresh files, so the unnamed orphans remain...
        assert!(att.join("orphan").exists());
        assert!(!att.join("aa").exists(), "a's private attachment is gone");
        assert!(att.join("shared").exists(), "b still references it");
        assert!(!crate::raw_source::cached_file_path(p.path(), "a:INBOX:1").exists());
        assert!(crate::raw_source::cached_file_path(p.path(), "b:INBOX:1").exists());
        // ...and an explicit zero-age sweep removes exactly the orphans.
        assert_eq!(
            p.sweep_orphaned_caches(std::time::Duration::ZERO).unwrap(),
            2
        );
        assert!(!att.join("orphan").exists() && !raw.join("deadbeef.eml").exists());
        assert!(att.join("shared").exists());
        assert!(crate::raw_source::cached_file_path(p.path(), "b:INBOX:1").exists());
    }

    // ---- sorted paging (#3043) ----

    fn seed_three(p: &crate::Profile) {
        use castellum_models::NewMessage;
        p.db.upsert_account(&Account {
            id: "a".into(),
            email: "a@x".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        p.db.upsert_mailbox(&Mailbox {
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
        for (uid, name, at) in [(1, "Zoe", 30), (2, "Àlex", 20), (3, "Bea", 10)] {
            p.db.upsert_message(&NewMessage {
                id: format!("a:INBOX:{uid}"),
                account_id: "a".into(),
                mailbox_id: "a:INBOX".into(),
                provider_uid: Some(uid),
                rfc822_message_id: Some(format!("<{uid}@x>")),
                subject: Some(format!("s{uid}")),
                from_name: Some(name.into()),
                from_address: Some("a@x".into()),
                sent_at: Some(at),
                snippet: None,
                size_bytes: None,
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
            .unwrap();
        }
    }

    #[test]
    fn list_messages_page_sorted_json_rejects_unknown_key() {
        let p = profile("sort-unknown");
        seed_three(&p);
        let err = p
            .list_messages_page_sorted_json("a", "a:INBOX", Some("size"), false, None, 10)
            .unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument), "{err:?}");
    }

    #[test]
    fn null_sort_key_equals_the_unsorted_page_byte_for_byte() {
        let p = profile("sort-null");
        seed_three(&p);
        let plain = p.list_messages_page_json("a", "a:INBOX", None, 10).unwrap();
        for key in [None, Some(""), Some("date")] {
            let sorted = p
                .list_messages_page_sorted_json("a", "a:INBOX", key, false, None, 10)
                .unwrap();
            assert_eq!(sorted, plain, "{key:?}");
        }
        let role_plain = p
            .list_messages_by_role_page_json("inbox", None, 10)
            .unwrap();
        let role_sorted = p
            .list_messages_by_role_page_sorted_json("inbox", None, false, None, 10)
            .unwrap();
        assert_eq!(role_sorted, role_plain);
    }

    #[test]
    fn a_cursor_from_another_sort_is_invalid_argument() {
        let p = profile("sort-cursor");
        seed_three(&p);
        let date_page = p
            .list_messages_page_sorted_json("a", "a:INBOX", None, false, None, 1)
            .unwrap();
        let next =
            serde_json::from_str::<serde_json::Value>(&date_page).unwrap()["next"].to_string();
        let err = p
            .list_messages_page_sorted_json("a", "a:INBOX", Some("from"), true, Some(&next), 1)
            .unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument), "{err:?}");
        // And the reverse: a text cursor replayed under date order.
        let from_page = p
            .list_messages_page_sorted_json("a", "a:INBOX", Some("from"), true, None, 1)
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&from_page).unwrap();
        assert_eq!(
            v["rows"][0]["from_name"], "Àlex",
            "accent folds before Bea and Zoe"
        );
        let text_next = v["next"].to_string();
        assert!(text_next.contains("\"key\""));
        let err = p
            .list_messages_page_sorted_json("a", "a:INBOX", None, false, Some(&text_next), 1)
            .unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgument), "{err:?}");
    }
}
