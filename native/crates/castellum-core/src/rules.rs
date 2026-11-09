// Castellum - rule execution via the operations queue (S40, MM-3).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Rule actions ALWAYS route through the existing `operations` queue
// (0005_operations.sql) — never direct provider I/O, never a new write
// path. Two shapes:
//
// * `run_rules_for_account` — the arrival pass, called once at the end of a
//   sync pass with the ids of mail that genuinely ARRIVED in an Inbox during
//   it (see `sync.rs`: never a first import, a UIDVALIDITY reset, another
//   folder or a decode-skip retry). It re-checks the Inbox restriction
//   itself and drops arrivals that are the destination copy of a move this
//   app queued (`Database::arrivals_following_own_moves`), so a rule can
//   never undo a user's move or ping-pong with another rule. DECISIONS.md
//   2026-09-30 (MM-3 review) records why.
// * `run_rule_now_batch` / `run_rule_now` — the explicit "Run Now" action,
//   evaluated against the rule's full current scope in bounded, id-ordered
//   batches (the app drives the batches off the main actor and can cancel
//   between them); `run_rule_now` stops at `RUN_NOW_MAX_MESSAGES`.
//
// Both funnel through `enqueue_actions`, which calls the SAME mutators a
// user-initiated action already uses (`Database::move_message_to_mailbox`,
// `set_flagged`, `mark_read`, `archive_message`, `delete_message`,
// `set_message_keyword`) rather than hand-rolling another way to insert an
// `operations` row. Mail in Trash, Junk, Drafts or Sent is never a
// candidate (`castellum_db::RULE_PROTECTED_ROLES`, applied in SQL).

use crate::{CoreError, Profile};
use castellum_db::Database;
use castellum_models::{FlagName, MessageHeader, RuleAction, RuleTrigger, SavedFilterKind};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// The most messages one Run Now acts on. A larger match set is reported
/// (`next_after` is set) and the user runs it again for the rest, so one
/// click can never queue an unbounded number of moves.
pub const RUN_NOW_MAX_MESSAGES: usize = 10_000;

/// The largest batch `run_rule_now_batch` accepts (the app asks for less):
/// one batch is one blocking call, so it must stay short.
pub const RUN_NOW_BATCH_MAX: i64 = 1_000;

fn db_io(e: castellum_db::DbError) -> CoreError {
    CoreError::Db(e.to_string())
}

fn to_json<T: Serialize>(value: &T) -> Result<String, CoreError> {
    serde_json::to_string(value).map_err(|_| CoreError::Io)
}

/// What one rule run did — the memo's §6 FFI comment's "Writes a JSON
/// RuleRunReport".
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RuleRunReport {
    pub filter_id: String,
    pub matched_count: usize,
    pub actions_enqueued: usize,
    /// MM-3: why this rule could not act (e.g. the account is read-only —
    /// "evaluation mode: ..."), so the failure is reported, never silent, and
    /// never stops the rules after it. Absent on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// MM-3 review, Run Now only: more candidates may sort after this
    /// message id — pass it back as `after` for the next batch. Absent when
    /// the run reached the end of the match set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_after: Option<String>,
    /// Run Now only, set together with `next_after`: how many candidates
    /// still sort after it, so the app can say "N more" instead of guessing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<usize>,
}

/// Role and owning account of every known mailbox in scope.
struct MailboxIndex {
    role: HashMap<String, String>,
    account: HashMap<String, String>,
}

impl MailboxIndex {
    fn role(&self, mailbox_id: &str) -> Option<&str> {
        self.role.get(mailbox_id).map(String::as_str)
    }
}

fn mailbox_index(db: &Database, account_id: Option<&str>) -> Result<MailboxIndex, CoreError> {
    let accounts: Vec<String> = match account_id {
        Some(id) => vec![id.to_string()],
        None => db
            .list_accounts()
            .map_err(db_io)?
            .into_iter()
            .map(|a| a.id)
            .collect(),
    };
    let mut index = MailboxIndex {
        role: HashMap::new(),
        account: HashMap::new(),
    };
    for account in accounts {
        for mailbox in db.list_mailboxes(&account).map_err(db_io)? {
            if let Some(role) = mailbox.role {
                index.role.insert(mailbox.id.clone(), role);
            }
            index.account.insert(mailbox.id, mailbox.account_id);
        }
    }
    Ok(index)
}

/// Checks, before anything is queued, the failures that would otherwise
/// leave a message half-processed (e.g. marked read but never moved): a
/// read-only account, and a Move whose folder is gone or belongs to another
/// account. MM-3 review: a rule's actions on one message either all get
/// queued or, for these causes, none do.
fn validate_actions(
    db: &Database,
    m: &MessageHeader,
    index: &MailboxIndex,
    actions: &[RuleAction],
) -> Result<(), CoreError> {
    // Tag actions alone never need writes: they change the local tag only.
    let needs_writes = actions
        .iter()
        .any(|a| !matches!(a, RuleAction::AddTag { .. } | RuleAction::RemoveTag { .. }));
    if needs_writes && !db.account_write_enabled(&m.account_id).map_err(db_io)? {
        return Err(CoreError::Db(castellum_db::EVALUATION_MODE_MSG.into()));
    }
    for action in actions {
        if let RuleAction::Move { mailbox_id } = action {
            if index.account.get(mailbox_id) != Some(&m.account_id) {
                return Err(CoreError::Db(format!(
                    "the rule's Move folder {mailbox_id} no longer exists in this account"
                )));
            }
        }
    }
    Ok(())
}

/// Applies one matched message's actions through the existing operations
/// queue, one queued op per action (never a combined/batched op kind — the
/// queue's existing shape is one op per effect). Returns how many ops were
/// actually queued, and the error that stopped the rest, if any: a partial
/// failure is counted as what it queued, never as zero.
fn enqueue_actions(
    db: &Database,
    m: &MessageHeader,
    index: &MailboxIndex,
    actions: &[RuleAction],
) -> (usize, Option<CoreError>) {
    if let Err(e) = validate_actions(db, m, index, actions) {
        return (0, Some(e));
    }
    let mut enqueued = 0usize;
    // Move/Archive/Trash delete the local `messages` row as part of
    // enqueueing (the optimistic-local-delete shape every move has), and
    // AddFlag/MarkRead look the row up by id — so every other action runs
    // first and the row-removing one runs last. Saving refuses a rule with
    // more than one row-removing action; `take(1)` only guards rows written
    // before that check existed.
    let (moves, rest): (Vec<_>, Vec<_>) = actions.iter().partition(|a| a.removes_message());
    let moves: Vec<_> = moves.into_iter().take(1).collect();
    for action in rest.into_iter().chain(moves) {
        // An action whose effect is already in place is a no-op, NOT an
        // error, NOT a queued op and not counted. A row-removing action that
        // would move the message to the folder it is already in: otherwise
        // `enqueue_move` would delete the local row and queue a move into the
        // same folder (silent row loss), and Trash refuses "already in
        // Trash". A read/star action on a message already in that state:
        // otherwise every re-run queues one op per message for nothing (FP20).
        let already_done = match action {
            RuleAction::Move { mailbox_id } => *mailbox_id == m.mailbox_id,
            RuleAction::Archive => index.role(&m.mailbox_id) == Some("archive"),
            RuleAction::Trash => index.role(&m.mailbox_id) == Some("trash"),
            RuleAction::MarkRead => m.is_read,
            RuleAction::MarkUnread => !m.is_read,
            RuleAction::AddFlag {
                flag: FlagName::Starred,
            } => m.is_starred,
            RuleAction::RemoveFlag {
                flag: FlagName::Starred,
            } => !m.is_starred,
            // Re-running must not queue the same keyword STORE again.
            RuleAction::AddTag { keyword } => {
                db.keyword_already_applied(&m.id, keyword).unwrap_or(false)
            }
            // Nothing to remove: no op, not counted (#3146).
            RuleAction::RemoveTag { keyword } => {
                !db.message_has_keyword(&m.id, keyword).unwrap_or(true)
            }
        };
        if already_done {
            continue;
        }
        let id = m.id.as_str();
        let result = match action {
            RuleAction::Move { mailbox_id } => db.move_message_to_mailbox(id, mailbox_id),
            RuleAction::AddFlag {
                flag: FlagName::Starred,
            } => db.set_flagged(id, true),
            RuleAction::MarkRead => db.mark_read(id, true),
            RuleAction::RemoveFlag {
                flag: FlagName::Starred,
            } => db.set_flagged(id, false),
            RuleAction::MarkUnread => db.mark_read(id, false),
            RuleAction::Archive => db.archive_message(id),
            RuleAction::Trash => db.delete_message(id),
            // Unlike AddFlag/MarkRead (explicit-intent writes that ERROR in
            // evaluation mode), `set_message_keyword` never errors for that
            // reason — it demotes to a local-only tag instead (S42
            // Assumption 5), so an on_sync AddTag rule still applies on a
            // read-only account.
            RuleAction::AddTag { keyword } => db.set_message_keyword(id, keyword, true).map(|_| ()),
            RuleAction::RemoveTag { keyword } => {
                db.set_message_keyword(id, keyword, false).map(|_| ())
            }
        };
        if let Err(e) = result {
            return (enqueued, Some(db_io(e)));
        }
        enqueued += 1;
    }
    (enqueued, None)
}

/// Totals of applying a rule to a set of matches.
#[derive(Default)]
struct ApplyTotals {
    enqueued: usize,
    failures: usize,
    first_error: Option<String>,
}

impl ApplyTotals {
    /// The report's error text: the first failure, and how many messages
    /// failed out of `matched` when more than one did.
    fn error(&self, matched: usize) -> Option<String> {
        self.first_error.as_ref().map(|e| {
            if self.failures > 1 {
                format!("{e} ({} of {matched} messages failed)", self.failures)
            } else {
                e.clone()
            }
        })
    }
}

/// Applies `actions` to every match, continuing past a failing message.
fn apply_to_matches(
    db: &Database,
    matches: &[MessageHeader],
    index: &MailboxIndex,
    actions: &[RuleAction],
    totals: &mut ApplyTotals,
) {
    for m in matches {
        let (queued, error) = enqueue_actions(db, m, index, actions);
        totals.enqueued += queued;
        if let Some(e) = error {
            totals.failures += 1;
            totals.first_error.get_or_insert_with(|| e.to_string());
        }
    }
}

/// Runs every enabled `kind = 'rule', trigger_kind = 'on_sync'` filter
/// scoped to `account_id` (its own rows plus every account-agnostic rule —
/// Assumption 7), in run order, against the mail that just arrived:
/// `new_message_ids`, narrowed here to messages sitting in an Inbox-role
/// folder that are not the destination copy of a move this app queued
/// (MM-3 review). The caller (`sync.rs`) already passes only genuine
/// arrivals — never a first import or a UIDVALIDITY reset — so this is a
/// second line of defence, not the only one. Any number of ids is safe:
/// the queries bind them in `castellum_db::RULE_ID_CHUNK` chunks.
/// Whether any enabled on_sync rule of `account_id` has a Body, AttachmentName,
/// AttachmentType or Header condition (anywhere in its nested groups). Those
/// read `message_fts` / `message_attachments` / `message_headers`, which stay
/// empty until a message's body has been fetched (bug hunt #50; Header: MM-1c).
pub fn on_sync_rules_need_bodies(db: &Database, account_id: &str) -> Result<bool, CoreError> {
    on_sync_rules_any(db, account_id, |f| {
        matches!(
            f,
            castellum_models::FilterField::Body
                | castellum_models::FilterField::AttachmentName
                | castellum_models::FilterField::AttachmentType
                | castellum_models::FilterField::Header(_)
        )
    })
}

/// Whether any enabled on_sync rule of `account_id` has a Header condition:
/// such rules also wait for the header capture, not just the body.
pub fn on_sync_rules_need_headers(db: &Database, account_id: &str) -> Result<bool, CoreError> {
    on_sync_rules_any(db, account_id, |f| {
        matches!(f, castellum_models::FilterField::Header(_))
    })
}

fn on_sync_rules_any(
    db: &Database,
    account_id: &str,
    pred: impl Fn(&castellum_models::FilterField) -> bool,
) -> Result<bool, CoreError> {
    fn needs(
        group: &castellum_models::FilterGroup,
        pred: &dyn Fn(&castellum_models::FilterField) -> bool,
    ) -> bool {
        group.conditions.iter().any(|c| pred(&c.field))
            || group.groups.iter().any(|g| needs(g, pred))
    }
    Ok(db
        .list_filters(Some(account_id))
        .map_err(db_io)?
        .iter()
        .any(|f| {
            f.kind == SavedFilterKind::Rule
                && f.trigger == Some(RuleTrigger::OnSync)
                && f.enabled
                && needs(&f.group, &pred)
        }))
}

/// Most parked arrivals one drain hands to the rules.
const DEFERRED_RULES_DRAIN_LIMIT: i64 = 2_000;

/// The sync pass's rules entry point (bug hunt #50). With no body-dependent
/// on_sync rule it is exactly [`run_rules_for_account`] on `arrivals`. With
/// one, arrivals that do not have their body yet are parked in `pending_rules`
/// (held back from EVERY rule, so rule order and `stop_processing` keep their
/// meaning) and run later by [`drain_deferred_rules`] once the body lands;
/// parked messages whose body has arrived since run now too.
pub fn run_rules_deferring_unhydrated(
    db: &Database,
    account_id: &str,
    arrivals: &[String],
) -> Result<Vec<RuleRunReport>, CoreError> {
    if !on_sync_rules_need_bodies(db, account_id)? {
        return run_rules_for_account(db, account_id, arrivals);
    }
    let mut have_bodies = db.messages_with_bodies(arrivals).map_err(db_io)?;
    let need_headers = on_sync_rules_need_headers(db, account_id)?;
    if need_headers {
        // MM-1c: a Header rule waits until the headers are captured as well.
        let captured = db.messages_with_header_capture(arrivals).map_err(db_io)?;
        have_bodies.retain(|id| captured.contains(id));
    }
    let (ready, waiting): (Vec<String>, Vec<String>) = arrivals
        .iter()
        .cloned()
        .partition(|id| have_bodies.contains(id));
    db.defer_rules_for(account_id, &waiting).map_err(db_io)?;
    let mut ids = ready;
    ids.extend(
        db.take_ready_deferred_rules_with(account_id, DEFERRED_RULES_DRAIN_LIMIT, need_headers)
            .map_err(db_io)?,
    );
    ids.sort_unstable();
    ids.dedup();
    run_rules_for_account(db, account_id, &ids)
}

/// Run the on_sync rules for parked arrivals whose body has now landed. Call
/// wherever bodies are stored (background hydration, body-on-open). Cheap when
/// nothing is parked. Best effort for the caller: it never fails a hydration.
pub fn drain_deferred_rules(
    db: &Database,
    account_id: &str,
) -> Result<Vec<RuleRunReport>, CoreError> {
    let need_headers = on_sync_rules_need_headers(db, account_id)?;
    let ids = db
        .take_ready_deferred_rules_with(account_id, DEFERRED_RULES_DRAIN_LIMIT, need_headers)
        .map_err(db_io)?;
    run_rules_for_account(db, account_id, &ids)
}

pub fn run_rules_for_account(
    db: &Database,
    account_id: &str,
    new_message_ids: &[String],
) -> Result<Vec<RuleRunReport>, CoreError> {
    if new_message_ids.is_empty() {
        return Ok(Vec::new());
    }
    let filters = db.list_filters(Some(account_id)).map_err(db_io)?;
    if !filters
        .iter()
        .any(|f| f.kind == SavedFilterKind::Rule && f.trigger == Some(RuleTrigger::OnSync))
    {
        return Ok(Vec::new());
    }
    let index = mailbox_index(db, Some(account_id))?;
    let moved_by_us = db
        .arrivals_following_own_moves(account_id, new_message_ids)
        .map_err(db_io)?;
    let mut reports = Vec::new();
    // MM-3: `list_filters` returns rules in run order. A message consumed by
    // a `stop_processing` rule is dropped from every later rule's candidates.
    let mut remaining: Vec<String> = new_message_ids
        .iter()
        .filter(|id| !moved_by_us.contains(*id))
        .cloned()
        .collect();
    for filter in filters {
        if filter.kind != SavedFilterKind::Rule
            || filter.trigger != Some(RuleTrigger::OnSync)
            || !filter.enabled
        {
            continue;
        }
        if remaining.is_empty() {
            break;
        }
        let evaluated = db.rule_candidates(
            &filter.group,
            Some(account_id),
            Some(&remaining),
            None,
            i64::MAX,
        );
        let matches: Vec<_> = match evaluated {
            // Arrival rules act on the Inbox only (the rule's scope for
            // arrivals); everything else is Run Now's job.
            Ok(all) => all
                .into_iter()
                .filter(|m| index.role(&m.mailbox_id) == Some("inbox"))
                .collect(),
            Err(e) => {
                // The rule could not even be evaluated: report it, consume
                // nothing (its stop protection cannot apply to unknown
                // matches), and let the other rules run.
                reports.push(RuleRunReport {
                    filter_id: filter.id,
                    matched_count: 0,
                    actions_enqueued: 0,
                    error: Some(db_io(e).to_string()),
                    next_after: None,
                    remaining: None,
                });
                continue;
            }
        };
        let mut totals = ApplyTotals::default();
        apply_to_matches(db, &matches, &index, &filter.actions, &mut totals);
        // A stop rule protects everything it MATCHED, even when an action
        // failed (e.g. its target folder was deleted): otherwise a later
        // catch-all rule would act on mail the stop rule meant to keep.
        if filter.stop_processing && !matches.is_empty() {
            let consumed: HashSet<&str> = matches.iter().map(|m| m.id.as_str()).collect();
            remaining.retain(|id| !consumed.contains(id.as_str()));
        }
        reports.push(RuleRunReport {
            filter_id: filter.id,
            matched_count: matches.len(),
            actions_enqueued: totals.enqueued,
            error: totals.error(matches.len()),
            next_after: None,
            remaining: None,
        });
    }
    Ok(reports)
}

/// Loads `filter_id`, which must be a rule (any trigger).
fn load_rule(db: &Database, filter_id: &str) -> Result<castellum_models::SavedFilter, CoreError> {
    let filter = db
        .get_filter(filter_id)
        .map_err(db_io)?
        .ok_or_else(|| CoreError::Db(format!("filter {filter_id} not found")))?;
    if filter.kind != SavedFilterKind::Rule {
        return Err(CoreError::Db(format!(
            "filter {filter_id} is not a rule (kind = {:?})",
            filter.kind
        )));
    }
    Ok(filter)
}

/// One Run Now batch (MM-3 review): the next `limit` (1..=
/// [`RUN_NOW_BATCH_MAX`]) candidates of rule `filter_id` whose message id
/// sorts after `after`, with its actions queued. `next_after` in the report
/// is where the following batch starts; `None` means the match set is done.
/// Id-ordered keyset paging means a batch whose actions leave the message
/// matching (Mark as read, Star) never sees it again, and one that moves
/// it simply finds it gone.
pub fn run_rule_now_batch(
    db: &Database,
    filter_id: &str,
    account_id: Option<&str>,
    after: Option<&str>,
    limit: i64,
) -> Result<RuleRunReport, CoreError> {
    let filter = load_rule(db, filter_id)?;
    let scope = account_id.or(filter.account_id.as_deref());
    let limit = limit.clamp(1, RUN_NOW_BATCH_MAX);
    let index = mailbox_index(db, scope)?;
    // Fetch one past the batch: a continuation exists only if a candidate
    // really follows the batch. Testing `len == limit` alone offered a
    // no-op Continue ("0 more still match") whenever the batch boundary
    // landed exactly on the end of the match set.
    let mut matches = db
        .rule_candidates(&filter.group, scope, None, after, limit + 1)
        .map_err(db_io)?;
    let more_follow = matches.len() as i64 > limit;
    matches.truncate(limit as usize);
    let next_after = more_follow
        .then(|| matches.last().map(|m| m.id.clone()))
        .flatten();
    let mut totals = ApplyTotals::default();
    apply_to_matches(db, &matches, &index, &filter.actions, &mut totals);
    // Counted AFTER the actions ran: a rule that moves mail has already
    // taken those messages out of what remains.
    // If the actions themselves took every later match away (a Move rule),
    // nothing is left: report done rather than a Continue that does nothing.
    let remaining = match &next_after {
        Some(cursor) => Some(
            db.count_rule_candidates_after(&filter.group, scope, Some(cursor))
                .map_err(db_io)?
                .max(0) as usize,
        ),
        None => None,
    };
    let (next_after, remaining) = match remaining {
        Some(0) => (None, None),
        r => (next_after, r),
    };
    Ok(RuleRunReport {
        filter_id: filter.id,
        matched_count: matches.len(),
        actions_enqueued: totals.enqueued,
        error: totals.error(matches.len()),
        next_after,
        remaining,
    })
}

/// The whole "Run now" / `Manual` action in one call (Assumption 4): every
/// batch of [`run_rule_now_batch`] until the match set ends or
/// [`RUN_NOW_MAX_MESSAGES`] messages were processed — then `next_after` says
/// where a further run would continue. The app drives the batches itself
/// (off the main actor, cancellable); this bounded form serves the FFI's
/// one-shot `castellum_run_rule_now` and tests. Errors if `filter_id` is
/// unknown or is not `kind = 'rule'`.
pub fn run_rule_now(
    db: &Database,
    filter_id: &str,
    account_id: Option<&str>,
) -> Result<RuleRunReport, CoreError> {
    const BATCH: i64 = 200;
    let mut total = RuleRunReport {
        filter_id: filter_id.to_string(),
        matched_count: 0,
        actions_enqueued: 0,
        error: None,
        next_after: None,
        remaining: None,
    };
    let mut after: Option<String> = None;
    let mut last_remaining: Option<usize> = None;
    loop {
        let left = RUN_NOW_MAX_MESSAGES.saturating_sub(total.matched_count) as i64;
        if left == 0 {
            total.next_after = after;
            total.remaining = last_remaining;
            break;
        }
        let batch =
            run_rule_now_batch(db, filter_id, account_id, after.as_deref(), left.min(BATCH))?;
        total.filter_id = batch.filter_id;
        total.matched_count += batch.matched_count;
        total.actions_enqueued += batch.actions_enqueued;
        if total.error.is_none() {
            total.error = batch.error;
        }
        last_remaining = batch.remaining;
        match batch.next_after {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    Ok(total)
}

/// Upsert request sent from the UI/tests — mirrors
/// `smart_mailboxes.rs::UpsertSmartMailboxRequest`'s shape.
#[derive(serde::Deserialize)]
struct UpsertFilterRequest {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
    name: String,
    kind: SavedFilterKind,
    group: castellum_models::FilterGroup,
    #[serde(default)]
    sort_field: Option<String>,
    #[serde(default = "default_true")]
    sort_desc: bool,
    #[serde(default)]
    trigger: Option<RuleTrigger>,
    #[serde(default)]
    actions: Vec<RuleAction>,
    /// MM-3: absent = leave as is (new rows: not stopping, enabled).
    #[serde(default)]
    stop_processing: Option<bool>,
    #[serde(default)]
    enabled: Option<bool>,
}

fn default_true() -> bool {
    true
}

/// The editor's dry run of unsaved conditions: the first matches and the
/// exact total, both with the same candidate rules the engine uses.
#[derive(Serialize)]
struct ConditionsPreview {
    matches: Vec<MessageHeader>,
    total: i64,
}

impl Profile {
    /// Create (`id` absent/empty) or update (`id` present) a saved filter
    /// from `{id?, account_id?, name, kind, group, sort_field?, sort_desc?,
    /// trigger?, actions?, stop_processing?, enabled?}` JSON — one
    /// transaction, options included — and write the stored row as JSON.
    pub fn upsert_filter_json(&self, json: &str) -> Result<String, CoreError> {
        let req: UpsertFilterRequest = serde_json::from_str(json)
            .map_err(|e| CoreError::Db(format!("bad upsert_filter json: {e}")))?;
        let stored = self
            .db
            .upsert_filter_with_options(
                castellum_db::SavedFilterUpsert {
                    id: req.id.as_deref(),
                    account_id: req.account_id.as_deref(),
                    name: &req.name,
                    kind: req.kind,
                    group: &req.group,
                    sort_field: req.sort_field.as_deref(),
                    sort_desc: req.sort_desc,
                    trigger: req.trigger,
                    actions: &req.actions,
                },
                req.stop_processing,
                req.enabled,
            )
            .map_err(db_io)?;
        to_json(&stored)
    }

    /// MM-3: run order from a JSON array of filter ids, e.g. `["a","b"]`.
    pub fn reorder_filters_json(&self, json: &str) -> Result<(), CoreError> {
        let ids: Vec<String> = serde_json::from_str(json)
            .map_err(|e| CoreError::Db(format!("bad reorder_filters json: {e}")))?;
        self.db.reorder_filters(&ids).map_err(db_io)
    }

    /// Saved filters scoped to `account_id` (or every filter when `None`) as
    /// a JSON array.
    pub fn list_filters_json(&self, account_id: Option<&str>) -> Result<String, CoreError> {
        to_json(&self.db.list_filters(account_id).map_err(db_io)?)
    }

    /// One saved filter as JSON, or JSON `null` when `id` is unknown.
    pub fn get_filter_json(&self, id: &str) -> Result<String, CoreError> {
        to_json(&self.db.get_filter(id).map_err(db_io)?)
    }

    /// Permanently delete a saved filter. Errors if `id` is unknown.
    pub fn delete_filter(&self, id: &str) -> Result<(), CoreError> {
        self.db.delete_filter(id).map_err(db_io)
    }

    /// Evaluates `group_json` (a `FilterGroup`) and writes matching
    /// `MessageHeader` rows as JSON — the smart-mailbox/ad-hoc read. Never
    /// touches the operations queue. A rule's dry run uses
    /// [`Profile::preview_rule_conditions_json`] instead, which applies the
    /// engine's candidate rules.
    pub fn evaluate_filter_json(
        &self,
        group_json: &str,
        account_id: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<String, CoreError> {
        let group: castellum_models::FilterGroup = serde_json::from_str(group_json)
            .map_err(|e| CoreError::Db(format!("bad filter group json: {e}")))?;
        to_json(
            &self
                .db
                .evaluate_filter(&group, account_id, None, true, limit, offset)
                .map_err(db_io)?,
        )
    }

    /// MM-3 review: the rule editor's dry run of UNSAVED conditions —
    /// `{"matches": [first `limit`], "total": exact count}` over the same
    /// candidates the engine acts on (Trash, Junk, Drafts and Sent left out
    /// in SQL, before the limit). Read-only.
    pub fn preview_rule_conditions_json(
        &self,
        group_json: &str,
        account_id: Option<&str>,
        limit: i64,
    ) -> Result<String, CoreError> {
        let group: castellum_models::FilterGroup = serde_json::from_str(group_json)
            .map_err(|e| CoreError::Db(format!("bad filter group json: {e}")))?;
        let (matches, total) = self
            .db
            .preview_rule_group(&group, account_id, None, true, limit)
            .map_err(db_io)?;
        to_json(&ConditionsPreview { matches, total })
    }

    /// Previews a saved `rule`-kind filter: the first `limit` candidates
    /// paired with the actions that WOULD apply, plus the exact `total` Run
    /// Now would act on — without enqueueing anything.
    pub fn preview_rule_json(
        &self,
        filter_id: &str,
        account_id: Option<&str>,
        limit: i64,
    ) -> Result<String, CoreError> {
        to_json(
            &self
                .db
                .preview_rule(filter_id, account_id, limit)
                .map_err(db_io)?,
        )
    }

    /// Runs exactly one rule (regardless of its `trigger_kind`) — the
    /// one-shot "Run now", bounded by [`RUN_NOW_MAX_MESSAGES`] — and writes
    /// the report as JSON.
    pub fn run_rule_now_json(
        &self,
        filter_id: &str,
        account_id: Option<&str>,
    ) -> Result<String, CoreError> {
        to_json(&run_rule_now(&self.db, filter_id, account_id)?)
    }

    /// One Run Now batch ([`run_rule_now_batch`]) as a JSON report.
    pub fn run_rule_now_batch_json(
        &self,
        filter_id: &str,
        account_id: Option<&str>,
        after: Option<&str>,
        limit: i64,
    ) -> Result<String, CoreError> {
        to_json(&run_rule_now_batch(
            &self.db, filter_id, account_id, after, limit,
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_profile;
    use castellum_models::{
        Account, Address, BoolOp, FilterCondition, FilterField, FilterOperator, FilterValue,
        Mailbox, NewMessage,
    };

    fn profile(tag: &str) -> Profile {
        let dir = castellum_testutil::scratch_root().join(format!(
            "castellum-rules-core-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        open_profile(&dir).unwrap()
    }

    fn seed(p: &Profile) {
        p.db.upsert_account(&Account {
            id: "a1".into(),
            email: "me@x.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        p.db.upsert_mailbox(&Mailbox {
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
        // Rule actions route through the same write-enabled mutators a user
        // action would use (Move/AddFlag/MarkRead all refuse in evaluation
        // mode) — leave the fresh account's fail-closed default behind so
        // these tests exercise the actual enqueue path, not just the
        // evaluation-mode refusal.
        p.db.set_account_write_enabled("a1", true).unwrap();
    }

    fn seed_message(p: &Profile, id: &str, uid: i64, subject: &str, is_read: bool) {
        p.db.upsert_message(&NewMessage {
            id: id.to_string(),
            account_id: "a1".into(),
            mailbox_id: "a1:INBOX".into(),
            provider_uid: Some(uid),
            rfc822_message_id: Some(format!("<{id}@x.test>")),
            subject: Some(subject.to_string()),
            from_name: None,
            from_address: Some("a@x.test".into()),
            sent_at: Some(1),
            snippet: None,
            size_bytes: None,
            has_attachments: false,
            attachments: None,
            is_read,
            is_starred: false,
            addresses: vec![Address {
                kind: "from".into(),
                name: None,
                address: "a@x.test".into(),
            }],
            text_body: None,
            html_body: None,
            sanitized_html: None,
            has_remote_content: false,
            references: vec![],
        })
        .unwrap();
    }

    fn invoice_rule_group() -> castellum_models::FilterGroup {
        castellum_models::FilterGroup {
            op: BoolOp::And,
            conditions: vec![FilterCondition {
                field: FilterField::Subject,
                operator: FilterOperator::Contains,
                value: FilterValue::Text("invoice".into()),
            }],
            groups: vec![],
        }
    }

    #[test]
    fn rerunning_mark_read_and_star_rules_queues_nothing_new() {
        // FP20: a flag action whose message is already in the target state
        // changes nothing, so it must queue nothing and count nothing.
        let p = profile("rerun_flags");
        seed(&p);
        seed_message(&p, "a1:INBOX:1", 1, "your invoice is ready", false);
        let ids = ["a1:INBOX:1".to_string()];
        for (name, action) in [
            ("Read invoices", RuleAction::MarkRead),
            (
                "Star invoices",
                RuleAction::AddFlag {
                    flag: FlagName::Starred,
                },
            ),
        ] {
            p.db.upsert_filter(castellum_db::SavedFilterUpsert {
                id: None,
                account_id: Some("a1"),
                name,
                kind: SavedFilterKind::Rule,
                group: &invoice_rule_group(),
                sort_field: None,
                sort_desc: true,
                trigger: Some(RuleTrigger::OnSync),
                actions: &[action],
            })
            .unwrap();
        }
        run_rules_for_account(&p.db, "a1", &ids).unwrap();
        let after_first = p.db.pending_operations("a1").unwrap().len();
        assert_eq!(after_first, 2, "first run: one mark_read + one flag");

        let reports = run_rules_for_account(&p.db, "a1", &ids).unwrap();
        assert_eq!(p.db.pending_operations("a1").unwrap().len(), after_first);
        assert!(reports.iter().all(|r| r.actions_enqueued == 0));
    }

    #[test]
    fn mark_read_rule_enqueues_exactly_one_operations_row_via_the_durable_queue() {
        // Deviation from the plan's literal Task 5 wording ("the message's
        // is_read flag is NOT flipped directly"): `Database::mark_read` is
        // the SAME mutator a user's explicit toggle already uses, and that
        // mutator applies the local flag optimistically *and* enqueues the
        // operations row in one transaction (repo.rs::mark_read_inner) —
        // exactly like every other write in this app (never a bare direct
        // provider write). Task 5 also says "do not hand-roll a fourth way
        // to insert an operations row", so this module calls that same
        // mutator rather than inventing a queue-only path. The invariant
        // that actually matters — and the one this test proves — is that
        // the DURABLE queue is never bypassed: exactly one `mark_read`
        // operations row is queued, and only replay (never this function)
        // talks to the provider. See the plan's Deviations log.
        let p = profile("mark_read");
        seed(&p);
        seed_message(&p, "a1:INBOX:1", 1, "your invoice is ready", false);

        let rule =
            p.db.upsert_filter(castellum_db::SavedFilterUpsert {
                id: None,
                account_id: Some("a1"),
                name: "Invoices",
                kind: SavedFilterKind::Rule,
                group: &invoice_rule_group(),
                sort_field: None,
                sort_desc: true,
                trigger: Some(RuleTrigger::OnSync),
                actions: &[RuleAction::MarkRead],
            })
            .unwrap();

        let reports = run_rules_for_account(&p.db, "a1", &["a1:INBOX:1".to_string()]).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].filter_id, rule.id);
        assert_eq!(reports[0].matched_count, 1);
        assert_eq!(reports[0].actions_enqueued, 1);

        // Exactly one queued mark_read op referencing the matched message —
        // never zero (bypassed) and never more than one (double-enqueued).
        let ops = p.db.pending_operations("a1").unwrap();
        let mark_read_ops: Vec<_> = ops.iter().filter(|o| o.kind == "mark_read").collect();
        assert_eq!(mark_read_ops.len(), 1);
        assert!(mark_read_ops[0].payload_json.contains("a1:INBOX:1"));
    }

    #[test]
    fn add_tag_rule_fires_on_ingest_and_calls_set_message_keyword() {
        // S42: an on_sync rule with AddTag applies the keyword via
        // set_message_keyword (write-enabled ⇒ one queued `keyword` op).
        let p = profile("add_tag");
        seed(&p);
        seed_message(&p, "a1:INBOX:1", 1, "your invoice is ready", false);

        let rule =
            p.db.upsert_filter(castellum_db::SavedFilterUpsert {
                id: None,
                account_id: Some("a1"),
                name: "Invoices",
                kind: SavedFilterKind::Rule,
                group: &invoice_rule_group(),
                sort_field: None,
                sort_desc: true,
                trigger: Some(RuleTrigger::OnSync),
                actions: &[RuleAction::AddTag {
                    keyword: "Invoices".to_string(),
                }],
            })
            .unwrap();

        let reports = run_rules_for_account(&p.db, "a1", &["a1:INBOX:1".to_string()]).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].filter_id, rule.id);
        assert_eq!(reports[0].matched_count, 1);
        assert_eq!(reports[0].actions_enqueued, 1);

        let map =
            p.db.keywords_for_messages(&["a1:INBOX:1".to_string()])
                .unwrap();
        assert_eq!(
            map.get("a1:INBOX:1").cloned().unwrap_or_default(),
            vec!["Invoices".to_string()]
        );
        let ops = p.db.pending_operations("a1").unwrap();
        let keyword_ops: Vec<_> = ops.iter().filter(|o| o.kind == "keyword").collect();
        assert_eq!(keyword_ops.len(), 1);
        assert!(keyword_ops[0].payload_json.contains("Invoices"));
    }

    #[test]
    fn re_running_an_add_tag_rule_queues_nothing_the_second_time() {
        let p = profile("add_tag_rerun");
        seed(&p);
        seed_message(&p, "a1:INBOX:1", 1, "your invoice is ready", false);
        p.db.upsert_filter(castellum_db::SavedFilterUpsert {
            id: None,
            account_id: Some("a1"),
            name: "Invoices",
            kind: SavedFilterKind::Rule,
            group: &invoice_rule_group(),
            sort_field: None,
            sort_desc: true,
            trigger: Some(RuleTrigger::OnSync),
            actions: &[RuleAction::AddTag {
                keyword: "Invoices".to_string(),
            }],
        })
        .unwrap();
        let ids = ["a1:INBOX:1".to_string()];
        let first = run_rules_for_account(&p.db, "a1", &ids).unwrap();
        assert_eq!(first[0].actions_enqueued, 1);
        let second = run_rules_for_account(&p.db, "a1", &ids).unwrap();
        assert_eq!(
            second[0].actions_enqueued, 0,
            "already tagged: nothing to do"
        );
        let ops = p.db.pending_operations("a1").unwrap();
        assert_eq!(
            ops.iter().filter(|o| o.kind == "keyword").count(),
            1,
            "one STORE, not two"
        );
    }

    fn upsert_tag_rule(
        p: &Profile,
        actions: &[RuleAction],
    ) -> Result<castellum_models::SavedFilter, castellum_db::DbError> {
        p.db.upsert_filter(castellum_db::SavedFilterUpsert {
            id: None,
            account_id: Some("a1"),
            name: "Invoices",
            kind: SavedFilterKind::Rule,
            group: &invoice_rule_group(),
            sort_field: None,
            sort_desc: true,
            trigger: Some(RuleTrigger::OnSync),
            actions,
        })
    }

    #[test]
    fn a_remove_tag_rule_removes_the_tag_queues_one_store_and_is_a_no_op_after() {
        // #3146: RemoveTag goes through set_message_keyword(add = false).
        let p = profile("remove_tag");
        seed(&p);
        seed_message(&p, "a1:INBOX:1", 1, "your invoice is ready", false);
        p.db.set_message_keyword("a1:INBOX:1", "Invoices", true)
            .unwrap();
        let ids = ["a1:INBOX:1".to_string()];
        let before = p.db.pending_operations("a1").unwrap().len();
        upsert_tag_rule(
            &p,
            &[RuleAction::RemoveTag {
                keyword: "invoices".to_string(),
            }],
        )
        .unwrap();

        let first = run_rules_for_account(&p.db, "a1", &ids).unwrap();
        assert_eq!(first[0].actions_enqueued, 1);
        let map = p.db.keywords_for_messages(&ids).unwrap();
        assert!(map
            .get("a1:INBOX:1")
            .cloned()
            .unwrap_or_default()
            .is_empty());
        let ops = p.db.pending_operations("a1").unwrap();
        assert_eq!(ops.len(), before + 1);
        let last = ops.last().unwrap();
        assert_eq!(last.kind, "keyword");
        assert!(
            last.payload_json.contains("\"add\":false"),
            "{}",
            last.payload_json
        );

        let second = run_rules_for_account(&p.db, "a1", &ids).unwrap();
        assert_eq!(second[0].actions_enqueued, 0, "nothing left to remove");
        assert_eq!(p.db.pending_operations("a1").unwrap().len(), before + 1);
    }

    #[test]
    fn saving_a_rule_that_adds_and_removes_the_same_tag_is_refused() {
        let p = profile("add_remove_same_tag");
        seed(&p);
        let err = upsert_tag_rule(
            &p,
            &[
                RuleAction::AddTag {
                    keyword: "Invoices".to_string(),
                },
                RuleAction::RemoveTag {
                    keyword: "invoices".to_string(),
                },
            ],
        )
        .unwrap_err();
        assert!(err.0.contains("add and remove the same tag"), "{err:?}");
        for bad in ["", "\\Seen", "two words"] {
            assert!(
                upsert_tag_rule(
                    &p,
                    &[RuleAction::RemoveTag {
                        keyword: bad.to_string()
                    }]
                )
                .is_err(),
                "keyword {bad:?} must be refused"
            );
        }
        // Different tags are fine.
        upsert_tag_rule(
            &p,
            &[
                RuleAction::AddTag {
                    keyword: "Done".to_string(),
                },
                RuleAction::RemoveTag {
                    keyword: "Todo".to_string(),
                },
            ],
        )
        .unwrap();
    }

    #[test]
    fn move_and_add_flag_on_the_same_match_each_enqueue_their_own_operations_row() {
        let p = profile("move_and_flag");
        seed(&p);
        p.db.upsert_mailbox(&Mailbox {
            id: "a1:Archive".into(),
            account_id: "a1".into(),
            path: "Archive".into(),
            name: "Archive".into(),
            role: Some("archive".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        p.db.set_account_write_enabled("a1", true).unwrap();
        seed_message(&p, "a1:INBOX:1", 1, "your invoice is ready", false);

        let rule =
            p.db.upsert_filter(castellum_db::SavedFilterUpsert {
                id: None,
                account_id: Some("a1"),
                name: "Invoices",
                kind: SavedFilterKind::Rule,
                group: &invoice_rule_group(),
                sort_field: None,
                sort_desc: true,
                trigger: Some(RuleTrigger::OnSync),
                actions: &[
                    RuleAction::Move {
                        mailbox_id: "a1:Archive".into(),
                    },
                    RuleAction::AddFlag {
                        flag: FlagName::Starred,
                    },
                ],
            })
            .unwrap();

        let reports = run_rules_for_account(&p.db, "a1", &["a1:INBOX:1".to_string()]).unwrap();
        assert_eq!(reports[0].filter_id, rule.id);
        assert_eq!(
            reports[0].actions_enqueued, 2,
            "two actions -> two distinct enqueue calls"
        );

        let ops = p.db.pending_operations("a1").unwrap();
        let move_count = ops.iter().filter(|o| o.kind == "move").count();
        let flag_count = ops.iter().filter(|o| o.kind == "flag").count();
        assert_eq!(move_count, 1, "move gets its own operations row");
        assert_eq!(
            flag_count, 1,
            "flag gets its own operations row, not combined with move"
        );
    }

    #[test]
    fn run_rules_for_account_ignores_smart_mailboxes_and_manual_rules() {
        let p = profile("ignores_non_on_sync");
        seed(&p);
        seed_message(&p, "a1:INBOX:1", 1, "your invoice is ready", false);

        // A smart mailbox with the same matching group — must never fire.
        p.db.upsert_filter(castellum_db::SavedFilterUpsert {
            id: None,
            account_id: Some("a1"),
            name: "Just browsing",
            kind: SavedFilterKind::SmartMailbox,
            group: &invoice_rule_group(),
            sort_field: None,
            sort_desc: true,
            trigger: None,
            actions: &[],
        })
        .unwrap();
        // A Manual-trigger rule — must never fire via the ingest hook.
        p.db.upsert_filter(castellum_db::SavedFilterUpsert {
            id: None,
            account_id: Some("a1"),
            name: "Manual only",
            kind: SavedFilterKind::Rule,
            group: &invoice_rule_group(),
            sort_field: None,
            sort_desc: true,
            trigger: Some(RuleTrigger::Manual),
            actions: &[RuleAction::MarkRead],
        })
        .unwrap();

        let reports = run_rules_for_account(&p.db, "a1", &["a1:INBOX:1".to_string()]).unwrap();
        assert!(
            reports.is_empty(),
            "neither a smart mailbox nor a Manual rule should run at ingest"
        );
    }

    #[test]
    fn run_rules_for_account_with_no_new_messages_is_a_no_op() {
        let p = profile("no_new_messages");
        seed(&p);
        let reports = run_rules_for_account(&p.db, "a1", &[]).unwrap();
        assert!(reports.is_empty());
    }

    #[test]
    fn run_rule_now_runs_a_manual_rule_against_the_full_mailbox() {
        let p = profile("run_now_manual");
        seed(&p);
        seed_message(&p, "a1:INBOX:1", 1, "your invoice is ready", false);

        let rule =
            p.db.upsert_filter(castellum_db::SavedFilterUpsert {
                id: None,
                account_id: Some("a1"),
                name: "Manual invoices",
                kind: SavedFilterKind::Rule,
                group: &invoice_rule_group(),
                sort_field: None,
                sort_desc: true,
                trigger: Some(RuleTrigger::Manual),
                actions: &[RuleAction::MarkRead],
            })
            .unwrap();

        let report = run_rule_now(&p.db, &rule.id, None).unwrap();
        assert_eq!(report.matched_count, 1);
        assert_eq!(report.actions_enqueued, 1);
    }

    #[test]
    fn run_rule_now_against_unknown_filter_errors_not_panics() {
        let p = profile("run_now_unknown");
        seed(&p);
        assert!(run_rule_now(&p.db, "nope", None).is_err());
    }

    #[test]
    fn json_wrappers_round_trip_over_the_boundary() {
        let p = profile("json_wrappers");
        seed(&p);
        seed_message(&p, "a1:INBOX:1", 1, "your invoice is ready", false);

        let created_json = p
            .upsert_filter_json(&format!(
                r#"{{"account_id":"a1","name":"Invoices","kind":"rule","group":{},"trigger":"on_sync","actions":["mark_read"]}}"#,
                serde_json::to_string(&invoice_rule_group()).unwrap()
            ))
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&created_json).unwrap();
        let id = v["id"].as_str().unwrap().to_string();

        assert!(p.list_filters_json(Some("a1")).unwrap().contains(&id));
        assert_ne!(p.get_filter_json(&id).unwrap(), "null");
        assert_eq!(p.get_filter_json("nope").unwrap(), "null");

        let group_json = serde_json::to_string(&invoice_rule_group()).unwrap();
        let evaluated = p
            .evaluate_filter_json(&group_json, Some("a1"), 50, 0)
            .unwrap();
        assert!(evaluated.contains("a1:INBOX:1"));

        let preview = p.preview_rule_json(&id, None, 50).unwrap();
        assert!(preview.contains("a1:INBOX:1"));
        assert!(preview.contains("mark_read"));

        let run_report = p.run_rule_now_json(&id, None).unwrap();
        assert!(run_report.contains("\"matched_count\":1"));

        p.delete_filter(&id).unwrap();
        assert_eq!(p.get_filter_json(&id).unwrap(), "null");
    }

    // ---- MM-3: ordering, stop-processing, enabled, extra actions ----

    fn make_rule(
        p: &Profile,
        name: &str,
        group: &castellum_models::FilterGroup,
        actions: &[RuleAction],
    ) -> String {
        p.db.upsert_filter(castellum_db::SavedFilterUpsert {
            id: None,
            account_id: Some("a1"),
            name,
            kind: SavedFilterKind::Rule,
            group,
            sort_field: None,
            sort_desc: true,
            trigger: Some(RuleTrigger::OnSync),
            actions,
        })
        .unwrap()
        .id
    }

    fn kinds(p: &Profile) -> Vec<String> {
        let mut k: Vec<String> =
            p.db.pending_operations("a1")
                .unwrap()
                .into_iter()
                .map(|o| o.kind)
                .collect();
        k.sort();
        k
    }

    #[test]
    fn rules_run_in_sort_order_and_a_stop_rule_consumes_the_message() {
        let p = profile("order_stop");
        seed(&p);
        seed_message(&p, "a1:INBOX:1", 1, "your invoice is ready", false);
        seed_message(&p, "a1:INBOX:2", 2, "another invoice", false);
        // Created alphabetically AFTER "Zed": order must come from sort_order, not name.
        let zed = make_rule(
            &p,
            "Zed star",
            &invoice_rule_group(),
            &[RuleAction::AddFlag {
                flag: FlagName::Starred,
            }],
        );
        let alpha = make_rule(
            &p,
            "Alpha mark read",
            &invoice_rule_group(),
            &[RuleAction::MarkRead],
        );
        p.db.set_rule_options(&zed, Some(true), None).unwrap();
        // Zed is first (created first => sort_order 0) and stops: Alpha never sees the messages.
        let reports =
            run_rules_for_account(&p.db, "a1", &["a1:INBOX:1".into(), "a1:INBOX:2".into()])
                .unwrap();
        assert_eq!(
            reports
                .iter()
                .map(|r| r.filter_id.as_str())
                .collect::<Vec<_>>(),
            vec![zed.as_str()]
        );
        assert_eq!(kinds(&p), vec!["flag", "flag"]);

        // Reorder so Alpha runs first (no stop): both rules now act.
        p.db.set_rule_options(&zed, Some(false), None).unwrap();
        p.reorder_filters_json(&format!(r#"["{alpha}","{zed}"]"#))
            .unwrap();
        let reports = run_rules_for_account(&p.db, "a1", &["a1:INBOX:1".into()]).unwrap();
        assert_eq!(
            reports
                .iter()
                .map(|r| r.filter_id.clone())
                .collect::<Vec<_>>(),
            vec![alpha, zed]
        );
    }

    #[test]
    fn a_disabled_rule_is_skipped_on_arrival_but_run_now_still_works() {
        let p = profile("disabled");
        seed(&p);
        seed_message(&p, "a1:INBOX:1", 1, "your invoice is ready", false);
        let id = make_rule(
            &p,
            "Invoices",
            &invoice_rule_group(),
            &[RuleAction::MarkRead],
        );
        p.db.set_rule_options(&id, None, Some(false)).unwrap();
        assert!(run_rules_for_account(&p.db, "a1", &["a1:INBOX:1".into()])
            .unwrap()
            .is_empty());
        assert!(kinds(&p).is_empty());
        let report = run_rule_now(&p.db, &id, Some("a1")).unwrap();
        assert_eq!(report.actions_enqueued, 1);
        assert_eq!(kinds(&p), vec!["mark_read"]);
    }

    #[test]
    fn extra_actions_use_the_existing_queued_mutators() {
        let p = profile("extra_actions");
        seed(&p);
        p.db.upsert_mailbox(&Mailbox {
            id: "a1:Archive".into(),
            account_id: "a1".into(),
            path: "Archive".into(),
            name: "Archive".into(),
            role: Some("archive".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        p.db.upsert_mailbox(&Mailbox {
            id: "a1:Trash".into(),
            account_id: "a1".into(),
            path: "Trash".into(),
            name: "Trash".into(),
            role: Some("trash".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        seed_message(&p, "a1:INBOX:1", 1, "your invoice is ready", true);
        // Starred as sync would see it (no queued op), so the unstar has an
        // effect to queue: an action already in place queues nothing (FP20).
        p.db.reconcile_message_flags("a1", "a1:INBOX:1", true, true)
            .unwrap();
        make_rule(
            &p,
            "Unread+unstar+archive",
            &invoice_rule_group(),
            &[
                RuleAction::MarkUnread,
                RuleAction::RemoveFlag {
                    flag: FlagName::Starred,
                },
                RuleAction::Archive,
            ],
        );
        let reports = run_rules_for_account(&p.db, "a1", &["a1:INBOX:1".into()]).unwrap();
        assert_eq!(reports[0].actions_enqueued, 3);
        assert_eq!(kinds(&p), vec!["flag", "mark_read", "move"]);
    }

    #[test]
    fn a_second_row_removing_action_is_refused_at_save_not_dropped() {
        // MM-3 review: the engine can only ever run ONE of Move/Archive/Trash
        // per match (the row is gone after it); a second one used to be
        // dropped silently. Now the save itself is refused, through both
        // the DB API and the JSON the app sends, and nothing is stored.
        let p = profile("one_remove");
        seed(&p);
        let err =
            p.db.upsert_filter(castellum_db::SavedFilterUpsert {
                id: None,
                account_id: Some("a1"),
                name: "Trash then archive",
                kind: SavedFilterKind::Rule,
                group: &invoice_rule_group(),
                sort_field: None,
                sort_desc: true,
                trigger: Some(RuleTrigger::OnSync),
                actions: &[RuleAction::Trash, RuleAction::Archive],
            })
            .unwrap_err();
        assert!(err.0.contains("only one action that moves"), "{err:?}");
        let json_err = p
            .upsert_filter_json(&format!(
                r#"{{"account_id":"a1","name":"R","kind":"rule","trigger":"on_sync","group":{},
                    "actions":[{{"move":{{"mailbox_id":"a1:INBOX"}}}},"trash"]}}"#,
                serde_json::to_string(&invoice_rule_group()).unwrap()
            ))
            .unwrap_err();
        assert!(json_err.to_string().contains("only one action that moves"));
        assert!(p.db.list_filters(Some("a1")).unwrap().is_empty());
    }

    #[test]
    fn json_upsert_takes_options_and_saving_an_uncompilable_rule_is_refused() {
        let p = profile("json_opts");
        seed(&p);
        let stored = p
            .upsert_filter_json(
                r#"{"account_id":"a1","name":"R","kind":"rule","trigger":"on_sync","stop_processing":true,"enabled":false,
                    "group":{"op":"and","conditions":[{"field":"subject","operator":"contains","value":"x"}],"groups":[]},
                    "actions":["mark_unread","archive",{"remove_flag":{"flag":"starred"}}]}"#,
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(v["stop_processing"], true);
        assert_eq!(v["enabled"], false);
        assert_eq!(v["actions"].as_array().unwrap().len(), 3);
        // gist_topic is reserved and cannot compile: refused at save, not on every sync.
        assert!(p
            .upsert_filter_json(
                r#"{"name":"G","kind":"rule","trigger":"on_sync",
                    "group":{"op":"and","conditions":[{"field":"gist_topic","operator":"in","value":["x"]}],"groups":[]},
                    "actions":["mark_read"]}"#
            )
            .is_err());
    }

    #[test]
    fn a_read_only_account_reports_the_refusal_and_does_not_stop_later_rules() {
        let p = profile("read_only_report");
        seed(&p);
        seed_message(&p, "a1:INBOX:1", 1, "your invoice is ready", false);
        let first = make_rule(&p, "First", &invoice_rule_group(), &[RuleAction::MarkRead]);
        let second = make_rule(&p, "Second", &invoice_rule_group(), &[RuleAction::MarkRead]);
        p.db.set_account_write_enabled("a1", false).unwrap(); // evaluation mode
        let reports = run_rules_for_account(&p.db, "a1", &["a1:INBOX:1".into()]).unwrap();
        assert_eq!(reports.len(), 2, "the second rule still ran (and reported)");
        assert_eq!(reports[0].filter_id, first);
        assert_eq!(reports[1].filter_id, second);
        for r in &reports {
            assert_eq!(r.actions_enqueued, 0);
            assert!(
                r.error.as_deref().unwrap_or("").contains("evaluation mode"),
                "{:?}",
                r.error
            );
        }
        assert!(
            kinds(&p).is_empty(),
            "nothing was queued for a read-only account"
        );
    }

    // ---- MM-3 review fixes ----

    fn add_mailbox(p: &Profile, name: &str, role: &str) {
        p.db.upsert_mailbox(&Mailbox {
            id: format!("a1:{name}"),
            account_id: "a1".into(),
            path: name.into(),
            name: name.into(),
            role: Some(role.into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
    }

    fn seed_in(p: &Profile, mailbox: &str, id: &str, uid: i64, subject: &str) {
        p.db.upsert_message(&NewMessage {
            id: id.to_string(),
            account_id: "a1".into(),
            mailbox_id: mailbox.to_string(),
            provider_uid: Some(uid),
            rfc822_message_id: Some(format!("<{id}@x.test>")),
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
    fn rules_never_act_on_trash_junk_drafts_or_sent() {
        let p = profile("protected");
        seed(&p);
        for (name, role) in [
            ("Trash", "trash"),
            ("Junk", "junk"),
            ("Drafts", "drafts"),
            ("Sent", "sent"),
            ("Archive", "archive"),
        ] {
            add_mailbox(&p, name, role);
        }
        seed_in(&p, "a1:INBOX", "a1:INBOX:1", 1, "invoice inbox");
        seed_in(&p, "a1:Trash", "a1:Trash:2", 2, "invoice trash");
        seed_in(&p, "a1:Junk", "a1:Junk:3", 3, "invoice junk");
        seed_in(&p, "a1:Drafts", "a1:Drafts:4", 4, "invoice draft");
        seed_in(&p, "a1:Sent", "a1:Sent:5", 5, "invoice sent");
        let id = make_rule(
            &p,
            "Trash invoices",
            &invoice_rule_group(),
            &[RuleAction::Trash],
        );
        let ids: Vec<String> = (1..=5)
            .map(|n| {
                [
                    "a1:INBOX:1",
                    "a1:Trash:2",
                    "a1:Junk:3",
                    "a1:Drafts:4",
                    "a1:Sent:5",
                ][n - 1]
                    .to_string()
            })
            .collect();

        // On arrival: only the Inbox message is a candidate — and no error from re-trashing.
        let reports = run_rules_for_account(&p.db, "a1", &ids).unwrap();
        assert_eq!(reports[0].matched_count, 1);
        assert_eq!(reports[0].actions_enqueued, 1);
        assert!(reports[0].error.is_none(), "{:?}", reports[0].error);

        // Run now (twice) never trips over what it already trashed / what sits in protected folders.
        let first = run_rule_now(&p.db, &id, Some("a1")).unwrap();
        assert!(first.error.is_none(), "{:?}", first.error);
        let second = run_rule_now(&p.db, &id, Some("a1")).unwrap();
        assert!(second.error.is_none());
        assert_eq!(second.matched_count, 0);
        // The preview mirrors that candidate set.
        let v: serde_json::Value =
            serde_json::from_str(&p.preview_rule_json(&id, Some("a1"), 50).unwrap()).unwrap();
        assert_eq!(v["matches"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn archive_in_the_archive_folder_and_move_to_own_mailbox_are_no_ops() {
        let p = profile("same_folder");
        seed(&p);
        add_mailbox(&p, "Archive", "archive");
        seed_in(
            &p,
            "a1:Archive",
            "a1:Archive:1",
            1,
            "invoice already archived",
        );
        let archive = make_rule(&p, "Archive", &invoice_rule_group(), &[RuleAction::Archive]);
        let to_self = make_rule(
            &p,
            "Move to self",
            &invoice_rule_group(),
            &[RuleAction::Move {
                mailbox_id: "a1:Archive".into(),
            }],
        );
        // Run Now reaches the Archive folder (arrival rules never do): the
        // archived message is a skipped no-op for both rules, not an error
        // and never a queued move into the folder it is already in.
        for id in [&archive, &to_self] {
            let report = run_rule_now(&p.db, id, Some("a1")).unwrap();
            assert_eq!(report.matched_count, 1);
            assert_eq!(report.actions_enqueued, 0);
            assert!(report.error.is_none(), "{:?}", report.error);
        }
        assert!(kinds(&p).is_empty(), "no queued move into the same folder");
    }

    #[test]
    fn a_failing_stop_rule_still_protects_what_it_matched() {
        let p = profile("stop_fails");
        seed(&p);
        seed_message(&p, "a1:INBOX:1", 1, "your invoice is ready", false);
        // Stop rule whose Move target does not exist -> its action fails.
        let stop = make_rule(
            &p,
            "Stop",
            &invoice_rule_group(),
            &[RuleAction::Move {
                mailbox_id: "a1:Gone".into(),
            }],
        );
        p.db.set_rule_options(&stop, Some(true), None).unwrap();
        // A later catch-all that would mark it read if it were still a candidate.
        make_rule(
            &p,
            "Catch-all",
            &invoice_rule_group(),
            &[RuleAction::MarkRead],
        );
        let reports = run_rules_for_account(&p.db, "a1", &["a1:INBOX:1".into()]).unwrap();
        assert_eq!(
            reports.len(),
            1,
            "the catch-all never saw the protected message"
        );
        assert_eq!(
            reports[0].matched_count, 1,
            "the real match count, not zero"
        );
        assert!(reports[0].error.is_some());
        assert!(kinds(&p).is_empty());
    }

    #[test]
    fn a_partial_failure_reports_real_counts_and_continues() {
        let p = profile("partial");
        seed(&p);
        seed_message(&p, "a1:INBOX:1", 1, "invoice one", false);
        seed_message(&p, "a1:INBOX:2", 2, "invoice two", false);
        // Make the FIRST message unmovable-by-id: delete its row after matching is impossible,
        // so instead point Move at a missing mailbox for both and assert counts/error text.
        make_rule(
            &p,
            "Broken move",
            &invoice_rule_group(),
            &[
                RuleAction::MarkRead,
                RuleAction::Move {
                    mailbox_id: "a1:Gone".into(),
                },
            ],
        );
        let reports =
            run_rules_for_account(&p.db, "a1", &["a1:INBOX:1".into(), "a1:INBOX:2".into()])
                .unwrap();
        assert_eq!(reports[0].matched_count, 2);
        // MM-3 review: the Move target is validated before anything is
        // queued, so MarkRead is NOT left behind half-applied.
        assert_eq!(reports[0].actions_enqueued, 0);
        assert!(kinds(&p).is_empty(), "nothing queued for either message");
        assert!(
            reports[0]
                .error
                .as_deref()
                .unwrap()
                .contains("2 of 2 messages failed"),
            "{:?}",
            reports[0].error
        );
    }

    #[test]
    fn run_now_reports_a_read_only_refusal_in_the_report() {
        let p = profile("run_now_readonly");
        seed(&p);
        seed_message(&p, "a1:INBOX:1", 1, "your invoice is ready", false);
        let id = make_rule(
            &p,
            "Invoices",
            &invoice_rule_group(),
            &[RuleAction::MarkRead],
        );
        p.db.set_account_write_enabled("a1", false).unwrap();
        let report = run_rule_now(&p.db, &id, Some("a1")).unwrap();
        assert!(
            report
                .error
                .as_deref()
                .unwrap_or("")
                .contains("evaluation mode"),
            "{:?}",
            report.error
        );
        assert_eq!(report.actions_enqueued, 0);
    }

    // ---- MM-3 review round 2: write safety ----

    /// An unread message in `mailbox` with an explicit Message-ID and
    /// server UID (either may be absent).
    fn seed_with(
        p: &Profile,
        mailbox: &str,
        id: &str,
        uid: Option<i64>,
        subject: &str,
        rfc: Option<&str>,
    ) {
        p.db.upsert_message(&NewMessage {
            id: id.to_string(),
            account_id: "a1".into(),
            mailbox_id: mailbox.to_string(),
            provider_uid: uid,
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

    fn seed_rfc(p: &Profile, mailbox: &str, id: &str, uid: i64, subject: &str, rfc: &str) {
        seed_with(p, mailbox, id, Some(uid), subject, Some(rfc));
    }

    fn is_read(p: &Profile, id: &str) -> bool {
        p.db.get_message_detail(id).unwrap().unwrap().header.is_read
    }

    #[test]
    fn arrival_rules_ignore_mail_arriving_outside_the_inbox() {
        // Ping-pong guard: a Move rule files invoices into Projects; when
        // that message later shows up in Projects as a "new" row (replay
        // never inserts the destination row), a second rule scoped to the
        // same conditions must not move it again. Arrival rules act on the
        // Inbox only.
        let p = profile("inbox_only");
        seed(&p);
        add_mailbox(&p, "Projects", "");
        seed_in(&p, "a1:Projects", "a1:Projects:7", 7, "invoice filed");
        make_rule(
            &p,
            "Back to inbox",
            &invoice_rule_group(),
            &[RuleAction::Move {
                mailbox_id: "a1:INBOX".into(),
            }],
        );
        let reports = run_rules_for_account(&p.db, "a1", &["a1:Projects:7".into()]).unwrap();
        assert_eq!(reports[0].matched_count, 0);
        assert!(kinds(&p).is_empty());
    }

    #[test]
    fn a_message_this_app_moved_is_not_a_new_arrival_when_it_comes_back() {
        // BLOCKER 1: the user restores a message from Trash to the Inbox
        // (a queued move, the source row deleted at once). The next sync
        // imports the Inbox copy as a brand-new row — it must NOT be handed
        // to a Trash rule again. A genuinely new invoice in the same pass
        // still is.
        let p = profile("own_move");
        seed(&p);
        add_mailbox(&p, "Trash", "trash");
        seed_rfc(
            &p,
            "a1:Trash",
            "a1:Trash:3",
            3,
            "invoice restored",
            "<restored@x.test>",
        );
        p.db.move_message_to_mailbox("a1:Trash:3", "a1:INBOX")
            .unwrap();
        // What the next sync stores: the restored copy under a new UID, and
        // one genuinely new invoice.
        seed_rfc(
            &p,
            "a1:INBOX",
            "a1:INBOX:40",
            40,
            "invoice restored",
            "<restored@x.test>",
        );
        seed_rfc(
            &p,
            "a1:INBOX",
            "a1:INBOX:41",
            41,
            "invoice new",
            "<new@x.test>",
        );
        make_rule(
            &p,
            "Trash invoices",
            &invoice_rule_group(),
            &[RuleAction::Trash],
        );

        let reports =
            run_rules_for_account(&p.db, "a1", &["a1:INBOX:40".into(), "a1:INBOX:41".into()])
                .unwrap();
        assert_eq!(reports[0].matched_count, 1, "only the new invoice");
        assert_eq!(reports[0].actions_enqueued, 1);
        let moves: Vec<String> =
            p.db.pending_operations("a1")
                .unwrap()
                .into_iter()
                .filter(|o| o.kind == "move")
                .map(|o| o.payload_json)
                .collect();
        assert_eq!(
            moves.len(),
            2,
            "the user's restore + the new invoice's trash"
        );
        assert!(moves.iter().any(|m| m.contains("<new@x.test>")));
        assert_eq!(
            moves
                .iter()
                .filter(|m| m.contains("<restored@x.test>"))
                .count(),
            1,
            "the restored message was never re-trashed"
        );
        assert!(p.db.get_message_detail("a1:INBOX:40").unwrap().is_some());
    }

    #[test]
    fn a_huge_arrival_id_list_is_chunked_under_the_sqlite_variable_limit() {
        // A first sync or a long offline gap can hand the rules pass tens of
        // thousands of ids; binding them in one `IN (...)` overran SQLite's
        // host-parameter limit (32766) and the whole pass errored.
        let p = profile("huge_ids");
        seed(&p);
        seed_message(&p, "a1:INBOX:1", 1, "your invoice is ready", false);
        make_rule(
            &p,
            "Invoices",
            &invoice_rule_group(),
            &[RuleAction::MarkRead],
        );
        let mut ids: Vec<String> = (2..40_000).map(|n| format!("a1:INBOX:{n}")).collect();
        ids.push("a1:INBOX:1".into());
        let reports = run_rules_for_account(&p.db, "a1", &ids).unwrap();
        assert!(reports[0].error.is_none(), "{:?}", reports[0].error);
        assert_eq!(reports[0].matched_count, 1);
        assert_eq!(kinds(&p), vec!["mark_read"]);
    }

    #[test]
    fn run_now_walks_the_match_set_in_id_ordered_batches_exactly_once() {
        // BLOCKER 2: Run Now is bounded batches, and a batch whose action
        // leaves the message matching (Mark as read) never revisits it.
        let p = profile("batches");
        seed(&p);
        for n in 1..=5 {
            seed_message(&p, &format!("a1:INBOX:{n}"), n, "invoice", false);
        }
        let id = make_rule(
            &p,
            "Invoices",
            &invoice_rule_group(),
            &[RuleAction::MarkRead],
        );
        let mut after: Option<String> = None;
        let mut sizes = Vec::new();
        loop {
            let r = run_rule_now_batch(&p.db, &id, Some("a1"), after.as_deref(), 2).unwrap();
            sizes.push(r.matched_count);
            match r.next_after {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        assert_eq!(sizes, vec![2, 2, 1]);
        assert_eq!(kinds(&p).len(), 5, "each message queued exactly once");
    }

    #[test]
    fn a_batch_ending_exactly_at_the_last_match_reports_done() {
        // Reviewer note: batch size == match count (or a multiple of it)
        // used to leave a cursor and "0 still match" behind a no-op Continue.
        let p = profile("exact_end");
        seed(&p);
        for n in 1..=4 {
            seed_message(&p, &format!("a1:INBOX:{n}"), n, "invoice", false);
        }
        let id = make_rule(
            &p,
            "Invoices",
            &invoice_rule_group(),
            &[RuleAction::MarkRead],
        );
        let whole = run_rule_now_batch(&p.db, &id, Some("a1"), None, 4).unwrap();
        assert_eq!(whole.matched_count, 4);
        assert!(whole.next_after.is_none() && whole.remaining.is_none());
        let first = run_rule_now_batch(&p.db, &id, Some("a1"), None, 2).unwrap();
        assert_eq!((first.matched_count, first.remaining), (2, Some(2)));
        let last =
            run_rule_now_batch(&p.db, &id, Some("a1"), first.next_after.as_deref(), 2).unwrap();
        assert_eq!(last.matched_count, 2);
        assert!(last.next_after.is_none() && last.remaining.is_none());
    }

    /// Seeds `id` in `mailbox` under an explicit Message-ID.
    fn seed_copy(p: &Profile, mailbox: &str, id: &str, uid: i64, rfc: &str) {
        p.db.upsert_message(&NewMessage {
            id: id.to_string(),
            account_id: "a1".into(),
            mailbox_id: mailbox.to_string(),
            provider_uid: Some(uid),
            rfc822_message_id: Some(rfc.to_string()),
            subject: Some("invoice".to_string()),
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
    fn run_now_acts_once_on_a_message_held_in_inbox_label_and_all_mail() {
        // R1 (b): Gmail keeps one message in several folders; Run Now must
        // count it once and queue one operation, and still match a message
        // that exists only in All Mail.
        let p = profile("gmail_copies");
        seed(&p);
        add_mailbox(&p, "Receipts", "other");
        add_mailbox(&p, "AllMail", "archive");
        seed_copy(&p, "a1:AllMail", "a1:AllMail:1", 1, "<both@x.test>");
        seed_copy(&p, "a1:Receipts", "a1:Receipts:1", 1, "<both@x.test>");
        seed_copy(&p, "a1:INBOX", "a1:INBOX:1", 1, "<both@x.test>");
        seed_copy(
            &p,
            "a1:AllMail",
            "a1:AllMail:2",
            2,
            "<only-archived@x.test>",
        );
        let id = make_rule(
            &p,
            "Invoices",
            &invoice_rule_group(),
            &[RuleAction::MarkRead],
        );
        assert_eq!(
            p.db.count_rule_candidates(&invoice_rule_group(), Some("a1"))
                .unwrap(),
            2
        );
        let report = run_rule_now(&p.db, &id, Some("a1")).unwrap();
        assert_eq!(report.matched_count, 2, "one per message, not per copy");
        assert_eq!(kinds(&p), vec!["mark_read", "mark_read"]);
        // Only one of those two operations touches the three-copy message.
        let ops = p.db.pending_operations("a1").unwrap();
        let on_both = ops
            .iter()
            .filter(|o| o.payload_json.contains("<both@x.test>") || o.payload_json.contains(":1\""))
            .count();
        assert_eq!(on_both, 1, "{ops:?}");
    }

    /// Bug hunt #49: a Move rule driven over several batches must not promote
    /// the Gmail All Mail copy of a message whose Inbox copy the previous batch
    /// just moved (the row-removing action deleted the preferred copy).
    #[test]
    fn move_rule_over_several_batches_acts_once_per_message_across_gmail_copies() {
        let p = profile("move_copies");
        seed(&p);
        add_mailbox(&p, "[Gmail]/All Mail", "archive");
        add_mailbox(&p, "Receipts", "other");
        for n in 1..=3 {
            let rfc = format!("<m{n}@x.test>");
            seed_copy(&p, "a1:INBOX", &format!("a1:INBOX:{n}"), n, &rfc);
            seed_copy(
                &p,
                "a1:[Gmail]/All Mail",
                &format!("a1:[Gmail]/All Mail:{n}"),
                n,
                &rfc,
            );
        }
        let id = make_rule(
            &p,
            "Receipts",
            &invoice_rule_group(),
            &[RuleAction::Move {
                mailbox_id: "a1:Receipts".into(),
            }],
        );
        let mut after: Option<String> = None;
        let (mut matched, mut batches) = (0, 0);
        loop {
            let r = run_rule_now_batch(&p.db, &id, Some("a1"), after.as_deref(), 1).unwrap();
            matched += r.matched_count;
            batches += 1;
            assert!(batches < 10, "runaway");
            match r.next_after {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        assert_eq!(matched, 3, "one per distinct message");
        let moves: Vec<String> =
            p.db.pending_operations("a1")
                .unwrap()
                .into_iter()
                .filter(|o| o.kind == "move")
                .map(|o| o.payload_json)
                .collect();
        assert_eq!(
            moves.len(),
            3,
            "exactly one move op per Message-ID: {moves:?}"
        );
        for n in 1..=3 {
            let needle = format!("<m{n}@x.test>");
            assert_eq!(
                moves.iter().filter(|m| m.contains(&needle)).count(),
                1,
                "{needle}"
            );
        }
        // And the shielded copies are not offered as candidates or counted.
        assert_eq!(
            p.db.count_rule_candidates(&invoice_rule_group(), Some("a1"))
                .unwrap(),
            0
        );
    }

    // ---- bug hunt #50: Body/Attachment rules and arrivals beyond the hydration cap ----

    fn body_rule_group() -> castellum_models::FilterGroup {
        castellum_models::FilterGroup {
            op: BoolOp::And,
            conditions: vec![FilterCondition {
                field: FilterField::Body,
                operator: FilterOperator::Contains,
                value: FilterValue::Text("invoice".into()),
            }],
            groups: vec![],
        }
    }

    /// An Inbox message; `body` Some = hydrated (stored body + FTS), None = descriptor only.
    fn seed_arrival(p: &Profile, n: i64, body: Option<&str>) -> String {
        let id = format!("a1:INBOX:{n}");
        p.db.upsert_message(&NewMessage {
            id: id.clone(),
            account_id: "a1".into(),
            mailbox_id: "a1:INBOX".into(),
            provider_uid: Some(n),
            rfc822_message_id: Some(format!("<arr{n}@x.test>")),
            subject: Some(format!("message {n}")),
            from_name: None,
            from_address: Some("a@x.test".into()),
            sent_at: Some(n),
            snippet: None,
            size_bytes: None,
            has_attachments: false,
            attachments: body.map(|_| vec![]), // a hydrated parse is authoritative (re-indexes FTS)
            is_read: false,
            is_starred: false,
            addresses: vec![],
            text_body: body.map(String::from),
            html_body: None,
            sanitized_html: None,
            has_remote_content: false,
            references: vec![],
        })
        .unwrap();
        id
    }

    #[test]
    fn body_rules_are_detected_in_nested_groups_only_when_enabled() {
        let p = profile("needs_body");
        seed(&p);
        assert!(!on_sync_rules_need_bodies(&p.db, "a1").unwrap());
        make_rule(
            &p,
            "Subject rule",
            &invoice_rule_group(),
            &[RuleAction::MarkRead],
        );
        assert!(!on_sync_rules_need_bodies(&p.db, "a1").unwrap());
        let nested = castellum_models::FilterGroup {
            op: BoolOp::Or,
            conditions: vec![],
            groups: vec![body_rule_group()],
        };
        make_rule(&p, "Nested body", &nested, &[RuleAction::MarkRead]);
        assert!(on_sync_rules_need_bodies(&p.db, "a1").unwrap());
    }

    #[test]
    fn arrivals_without_a_body_wait_for_it_and_then_run_every_rule_exactly_once() {
        let p = profile("deferred");
        seed(&p);
        make_rule(&p, "Invoices", &body_rule_group(), &[RuleAction::MarkRead]);
        // 30 arrivals; only the first 25 have been hydrated (the per-pass cap).
        let ids: Vec<String> = (1..=30)
            .map(|n| seed_arrival(&p, n, (n <= 25).then_some("your invoice is attached")))
            .collect();
        let first = run_rules_deferring_unhydrated(&p.db, "a1", &ids).unwrap();
        assert_eq!(
            first[0].matched_count, 25,
            "only hydrated mail is judged now"
        );
        assert_eq!(
            p.db.pending_rules_count("a1").unwrap(),
            5,
            "the rest are parked"
        );
        // Nothing hydrated yet: a drain does nothing.
        assert!(drain_deferred_rules(&p.db, "a1").unwrap().is_empty());
        // Their bodies land (background hydration / body-on-open).
        for n in 26..=30 {
            seed_arrival(&p, n, Some("another invoice for you"));
        }
        let drained = drain_deferred_rules(&p.db, "a1").unwrap();
        assert_eq!(drained[0].matched_count, 5);
        assert_eq!(p.db.pending_rules_count("a1").unwrap(), 0);
        // Exactly once each: a second drain finds nothing, and every message got one op.
        assert!(drain_deferred_rules(&p.db, "a1").unwrap().is_empty());
        let ops = p.db.pending_operations("a1").unwrap();
        assert_eq!(ops.iter().filter(|o| o.kind == "mark_read").count(), 30);
    }

    #[test]
    fn without_a_body_rule_nothing_is_parked_and_behaviour_is_unchanged() {
        let p = profile("not_deferred");
        seed(&p);
        make_rule(
            &p,
            "Subject",
            &invoice_rule_group(),
            &[RuleAction::MarkRead],
        );
        let ids: Vec<String> = (1..=3).map(|n| seed_arrival(&p, n, None)).collect();
        for (n, id) in (1..=3).zip(&ids) {
            p.db.upsert_message(&NewMessage {
                subject: Some("invoice".into()),
                ..msg_like(&p, id, n)
            })
            .unwrap();
        }
        let reports = run_rules_deferring_unhydrated(&p.db, "a1", &ids).unwrap();
        assert_eq!(reports[0].matched_count, 3);
        assert_eq!(p.db.pending_rules_count("a1").unwrap(), 0);
    }

    /// A message row identical to `seed_arrival`'s, for re-upserting with changes.
    fn msg_like(_p: &Profile, id: &str, n: i64) -> NewMessage {
        NewMessage {
            id: id.to_string(),
            account_id: "a1".into(),
            mailbox_id: "a1:INBOX".into(),
            provider_uid: Some(n),
            rfc822_message_id: Some(format!("<arr{n}@x.test>")),
            subject: None,
            from_name: None,
            from_address: Some("a@x.test".into()),
            sent_at: Some(n),
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
        }
    }

    #[test]
    fn continue_resumes_from_the_cursor_and_a_fresh_run_starts_over() {
        // R1 (a): Mark as read leaves a message matching, so a restart from
        // the top never reaches the tail. Continue passes the cursor back.
        let p = profile("continue");
        seed(&p);
        for n in 1..=5 {
            seed_message(&p, &format!("a1:INBOX:{n}"), n, "invoice", false);
        }
        let id = make_rule(
            &p,
            "Invoices",
            &invoice_rule_group(),
            &[RuleAction::MarkRead],
        );
        // First "click": a capped run of one batch of two.
        let first = run_rule_now_batch(&p.db, &id, Some("a1"), None, 2).unwrap();
        assert_eq!((first.matched_count, first.remaining), (2, Some(3)));
        // A fresh Run Now starts over and re-visits the same first two.
        let fresh = run_rule_now_batch(&p.db, &id, Some("a1"), None, 2).unwrap();
        assert_eq!(fresh.next_after, first.next_after);
        // Continue: batches two and three reach messages 3..5.
        let second =
            run_rule_now_batch(&p.db, &id, Some("a1"), first.next_after.as_deref(), 2).unwrap();
        assert_eq!((second.matched_count, second.remaining), (2, Some(1)));
        let third =
            run_rule_now_batch(&p.db, &id, Some("a1"), second.next_after.as_deref(), 2).unwrap();
        assert_eq!((third.matched_count, third.remaining), (1, None));
        assert!(third.next_after.is_none());
        let touched: std::collections::HashSet<String> =
            p.db.pending_operations("a1")
                .unwrap()
                .into_iter()
                .map(|o| o.payload_json)
                .collect();
        for n in 1..=5 {
            assert!(
                touched
                    .iter()
                    .any(|j| j.contains(&format!("a1:INBOX:{n}\""))),
                "message {n} reached"
            );
        }
    }

    #[test]
    fn run_now_stops_at_its_cap_and_says_where_to_continue() {
        let p = profile("cap");
        seed(&p);
        let n = (RUN_NOW_MAX_MESSAGES + 3) as i64;
        let group = invoice_rule_group();
        for i in 1..=n {
            seed_message(&p, &format!("a1:INBOX:{i:06}"), i, "invoice", true);
        }
        let id = make_rule(&p, "Invoices", &group, &[RuleAction::MarkRead]);
        let report = run_rule_now(&p.db, &id, Some("a1")).unwrap();
        assert_eq!(report.matched_count, RUN_NOW_MAX_MESSAGES);
        let next = report.next_after.expect("capped: more remain");
        let rest = run_rule_now_batch(&p.db, &id, Some("a1"), Some(&next), 200).unwrap();
        assert_eq!(rest.matched_count, 3);
        assert!(rest.next_after.is_none());
        assert_eq!(p.db.count_rule_candidates(&group, Some("a1")).unwrap(), n);
    }

    #[test]
    fn preview_counts_exactly_with_protected_folders_excluded_before_the_limit() {
        // BLOCKER 2: the old preview took `limit` rows and only THEN dropped
        // Trash/Junk/Drafts/Sent, so its count was neither exact nor right.
        let p = profile("preview_count");
        seed(&p);
        add_mailbox(&p, "Trash", "trash");
        for n in 1..=6 {
            seed_in(&p, "a1:Trash", &format!("a1:Trash:{n}"), n, "invoice old");
        }
        for n in 10..=12 {
            seed_in(&p, "a1:INBOX", &format!("a1:INBOX:{n}"), n, "invoice");
        }
        let id = make_rule(
            &p,
            "Invoices",
            &invoice_rule_group(),
            &[RuleAction::MarkRead],
        );
        let v: serde_json::Value =
            serde_json::from_str(&p.preview_rule_json(&id, Some("a1"), 2).unwrap()).unwrap();
        assert_eq!(v["total"], 3);
        assert_eq!(v["matches"].as_array().unwrap().len(), 2);
        for m in v["matches"].as_array().unwrap() {
            assert_eq!(m["mailbox_id"], "a1:INBOX");
        }
        // The editor's dry run of unsaved conditions agrees.
        let v: serde_json::Value = serde_json::from_str(
            &p.preview_rule_conditions_json(
                &serde_json::to_string(&invoice_rule_group()).unwrap(),
                Some("a1"),
                2,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(v["total"], 3);
        assert_eq!(v["matches"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_missing_move_folder_queues_nothing_for_the_message() {
        // SHOULD-FIX: Mark as read used to be queued, then the Move failed,
        // and the report said 0 — the message was left half-processed.
        let p = profile("atomic_move");
        seed(&p);
        seed_message(&p, "a1:INBOX:1", 1, "invoice", false);
        make_rule(
            &p,
            "Read and file",
            &invoice_rule_group(),
            &[
                RuleAction::MarkRead,
                RuleAction::Move {
                    mailbox_id: "a1:Gone".into(),
                },
            ],
        );
        let reports = run_rules_for_account(&p.db, "a1", &["a1:INBOX:1".into()]).unwrap();
        assert_eq!(reports[0].actions_enqueued, 0);
        assert!(reports[0]
            .error
            .as_deref()
            .unwrap()
            .contains("no longer exists"));
        assert!(kinds(&p).is_empty());
        assert!(
            !is_read(&p, "a1:INBOX:1"),
            "Mark as read was not applied either"
        );
    }

    #[test]
    fn a_read_only_account_applies_no_local_tag_before_refusing_the_move() {
        // AddTag alone demotes to a local tag in evaluation mode; combined
        // with a Move that must refuse, nothing at all is applied.
        let p = profile("atomic_readonly");
        seed(&p);
        add_mailbox(&p, "Archive", "archive");
        seed_message(&p, "a1:INBOX:1", 1, "invoice", false);
        make_rule(
            &p,
            "Tag and archive",
            &invoice_rule_group(),
            &[
                RuleAction::AddTag {
                    keyword: "Invoices".into(),
                },
                RuleAction::Archive,
            ],
        );
        p.db.set_account_write_enabled("a1", false).unwrap();
        let reports = run_rules_for_account(&p.db, "a1", &["a1:INBOX:1".into()]).unwrap();
        assert_eq!(reports[0].actions_enqueued, 0);
        assert!(reports[0]
            .error
            .as_deref()
            .unwrap()
            .contains("evaluation mode"));
        let tags = p.db.keywords_for_messages(&["a1:INBOX:1".into()]).unwrap();
        assert!(
            tags.get("a1:INBOX:1").is_none_or(|t| t.is_empty()),
            "{tags:?}"
        );
    }

    #[test]
    fn a_partially_applied_action_list_counts_what_it_queued() {
        // When a later action fails for a reason validation cannot foresee
        // (here the message has no server UID, so the Move cannot be
        // queued), the report counts the op that WAS queued, never 0.
        let p = profile("partial_count");
        seed(&p);
        add_mailbox(&p, "Archive", "archive");
        seed_with(
            &p,
            "a1:INBOX",
            "a1:INBOX:1",
            None,
            "invoice",
            Some("<l@x.test>"),
        );
        make_rule(
            &p,
            "Read and archive",
            &invoice_rule_group(),
            &[RuleAction::MarkRead, RuleAction::Archive],
        );
        let reports = run_rules_for_account(&p.db, "a1", &["a1:INBOX:1".into()]).unwrap();
        assert_eq!(
            reports[0].actions_enqueued, 1,
            "the mark_read that was queued"
        );
        assert!(reports[0].error.is_some());
        assert_eq!(kinds(&p), vec!["mark_read"]);
    }

    #[test]
    fn saving_a_rule_writes_its_options_in_the_same_transaction() {
        let p = profile("upsert_tx");
        seed(&p);
        let group = serde_json::to_string(&invoice_rule_group()).unwrap();
        let stored = p
            .upsert_filter_json(&format!(
                r#"{{"account_id":"a1","name":"R","kind":"rule","trigger":"on_sync","group":{group},
                    "actions":["mark_read"],"stop_processing":true,"enabled":false}}"#
            ))
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&stored).unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        assert_eq!(v["stop_processing"], true);
        assert_eq!(v["enabled"], false);
        // A refused update (two row-removing actions) changes nothing — not
        // the conditions, not the name, not the options.
        assert!(p
            .upsert_filter_json(&format!(
                r#"{{"id":"{id}","account_id":"a1","name":"Renamed","kind":"rule","trigger":"on_sync",
                    "group":{group},"actions":["archive","trash"],"enabled":true}}"#
            ))
            .is_err());
        let f = p.db.get_filter(&id).unwrap().unwrap();
        assert_eq!(f.name, "R");
        assert!(!f.enabled);
        assert!(f.stop_processing);
    }

    /// SECURITY (MM-1c): a Header rule is a body-dependent rule and waits for the
    /// header capture, then applies exactly once.
    #[test]
    fn header_rules_wait_for_the_header_capture() {
        let p = profile("header_deferred");
        seed(&p);
        let group = castellum_models::FilterGroup {
            op: BoolOp::And,
            conditions: vec![FilterCondition {
                field: FilterField::Header("List-Id".into()),
                operator: FilterOperator::Is,
                value: FilterValue::Bool(false),
            }],
            groups: vec![],
        };
        make_rule(&p, "No list", &group, &[RuleAction::MarkRead]);
        assert!(on_sync_rules_need_bodies(&p.db, "a1").unwrap());
        assert!(on_sync_rules_need_headers(&p.db, "a1").unwrap());
        // Body present but headers not captured yet: still parked.
        let id = seed_arrival(&p, 1, Some("hello"));
        let first = run_rules_deferring_unhydrated(&p.db, "a1", std::slice::from_ref(&id)).unwrap();
        assert!(first.iter().all(|r| r.matched_count == 0));
        assert_eq!(p.db.pending_rules_count("a1").unwrap(), 1);
        assert!(drain_deferred_rules(&p.db, "a1").unwrap().is_empty());
        // Capture lands (no List-Id kept): the rule applies once.
        p.db.replace_message_headers(&id, &[], castellum_models::HeaderCaptureSource::Body, false)
            .unwrap();
        let drained = drain_deferred_rules(&p.db, "a1").unwrap();
        assert_eq!(drained[0].matched_count, 1);
        assert_eq!(p.db.pending_rules_count("a1").unwrap(), 0);
        assert!(drain_deferred_rules(&p.db, "a1").unwrap().is_empty());
    }
}
