// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//! Suggestions you accept (AW2): the producers that PROPOSE work items and
//! state changes, and the profile-level list/accept/dismiss/count calls.
//!
//! Marc's rule (DECISIONS.md, 2026-10-01): nothing is ever created
//! automatically from mail. A producer only writes a `suggestions` row; the
//! user accepts it (creating the work item, atomically, in
//! `Database::accept_suggestion_at`) or dismisses it.
//!
//! Producers (all idempotent on a dedupe key, all clock-injected, none calls
//! a model, none runs on the launch path or the main thread):
//! - [`propose_from_extractions`] (a): one `work_item` suggestion per
//!   `action_item` row S54 already stored. Called from the AI executor right
//!   after it stored those rows.
//! - [`propose_after_send`] (b): one `waiting_for_reply` suggestion when the
//!   send executor has delivered a message, due [`FOLLOW_UP_WORKING_DAYS`]
//!   working days later. Nothing for a message sent only to yourself.
//! - [`propose_reply_state_changes`] (c): when a genuine inbound message
//!   arrives in a thread that has a waiting work item, one `state_change`
//!   (waiting -> needs_reply) suggestion. The work item is NOT touched.
//!
//! Evidence is ids only. The dedupe key for an extraction proposal hashes the
//! item text (the extraction row id changes every re-run, because rows are
//! replaced wholesale), so a re-extraction of the same item does not
//! re-propose it and a dismissed item stays dismissed.

use crate::{CoreError, Profile};
use castellum_db::{
    Database, NewSuggestion, Suggestion, SuggestionAccepted, SuggestionFilter, WorkItemFilter,
};
use castellum_models::Draft;
use sha2::{Digest, Sha256};

/// Working days after a send at which a waiting-for-reply item is proposed
/// to fall due (Saturday and Sunday do not count; UTC).
pub const FOLLOW_UP_WORKING_DAYS: u32 = 3;

const SECS_PER_DAY: i64 = 86_400;

fn db_err(e: castellum_db::DbError) -> CoreError {
    CoreError::Db(e.to_string())
}

fn to_json<T: serde::Serialize>(value: &T) -> Result<String, CoreError> {
    serde_json::to_string(value).map_err(|_| CoreError::Io)
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `epoch` moved forward by `days` working days (Mon-Fri, UTC), keeping the
/// time of day. A weekend start counts from the next weekday.
pub fn add_working_days(epoch: i64, days: u32) -> i64 {
    let mut t = epoch;
    let mut left = days;
    while left > 0 {
        t += SECS_PER_DAY;
        // 1970-01-01 was a Thursday: day 0 -> Thu, so (day + 3) % 7 is
        // 0 = Monday .. 5 = Saturday, 6 = Sunday.
        let weekday = (t.div_euclid(SECS_PER_DAY) + 3).rem_euclid(7);
        if weekday < 5 {
            left -= 1;
        }
    }
    t
}

/// `YYYY-MM-DD` to epoch seconds at 00:00 UTC; `None` if malformed.
fn parse_iso_date(s: &str) -> Option<i64> {
    let mut it = s.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * SECS_PER_DAY)
}

/// Stable short hash of an item's text (case/whitespace-insensitive).
fn text_key(text: &str) -> String {
    let norm = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let digest = Sha256::digest(norm.as_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// (a) One `work_item` suggestion per stored `action_item` of `thread_id`.
/// Reads rows S54 already stored; calls no model. The proposed title is the
/// extraction's text, the proposed due date its structured date when it has
/// one. Returns how many NEW suggestions were created.
pub fn propose_from_extractions(
    db: &Database,
    thread_id: &str,
    now: i64,
) -> Result<usize, CoreError> {
    let mut created = 0;
    for row in db.get_thread_extractions(thread_id).map_err(db_err)? {
        if row.kind != "action_item" {
            continue;
        }
        let title = row.text.trim();
        if title.is_empty() {
            continue;
        }
        let due_at = row
            .structured_value
            .as_deref()
            .and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok())
            .and_then(|v| {
                v.get("date")
                    .and_then(|d| d.as_str())
                    .and_then(parse_iso_date)
            });
        let payload = serde_json::json!({ "title": title, "due_at": due_at }).to_string();
        let key = format!("extraction:{thread_id}:{}", text_key(title));
        let made = db
            .propose_suggestion_at(
                &NewSuggestion {
                    kind: "work_item",
                    payload_json: &payload,
                    message_id: row.message_id.as_deref(),
                    thread_id: Some(thread_id),
                    extraction_id: Some(row.id),
                    origin: "ai_extraction",
                    dedupe_key: &key,
                    ..Default::default()
                },
                now,
            )
            .map_err(db_err)?;
        created += usize::from(made.is_some());
    }
    Ok(created)
}

/// (b) After a successful send: propose a waiting-for-reply item for the
/// message's thread, due [`FOLLOW_UP_WORKING_DAYS`] working days after `now`.
///
/// `own_addresses` are the sender's addresses (account + identities); a
/// message whose every recipient is one of them (or that has none) gets no
/// proposal. `sent_rfc822_message_id` is the delivered message's Message-ID:
/// the sent copy usually has not synced yet, so it is stored as evidence and
/// resolved to a message/thread later. A reply's thread is resolved at once
/// from `in_reply_to`. Idempotent per draft.
pub fn propose_after_send(
    db: &Database,
    draft: &Draft,
    own_addresses: &[String],
    sent_rfc822_message_id: Option<&str>,
    now: i64,
) -> Result<Option<Suggestion>, CoreError> {
    let norm = |e: &str| e.trim().to_ascii_lowercase();
    let recipients: Vec<String> = draft
        .to
        .iter()
        .chain(&draft.cc)
        .chain(&draft.bcc)
        .map(|a| norm(&a.email))
        .filter(|e| !e.is_empty())
        .collect();
    let mut own: Vec<String> = own_addresses.iter().map(|a| norm(a)).collect();
    if let Some(f) = draft.from_email.as_deref() {
        own.push(norm(f));
    }
    if recipients.is_empty() || recipients.iter().all(|r| own.contains(r)) {
        return Ok(None);
    }
    let (mut message_id, mut thread_id) = (None, None);
    if let Some(irt) = draft.in_reply_to.as_deref().filter(|s| !s.is_empty()) {
        if let Some((m, t)) = db
            .message_by_rfc822(&draft.account_id, irt)
            .map_err(db_err)?
        {
            message_id = Some(m);
            thread_id = t;
        }
    }
    let subject = draft.subject.trim();
    let title = if subject.is_empty() {
        "(no subject)"
    } else {
        subject
    };
    let payload = serde_json::json!({
        "title": title,
        "due_at": add_working_days(now, FOLLOW_UP_WORKING_DAYS),
        // Late evidence (the Sent copy) is accepted only from this account.
        "account_id": draft.account_id,
    })
    .to_string();
    let key = format!("sent:{}:{}", draft.account_id, draft.id);
    db.propose_suggestion_at(
        &NewSuggestion {
            kind: "waiting_for_reply",
            payload_json: &payload,
            message_id: message_id.as_deref(),
            thread_id: thread_id.as_deref(),
            rfc822_message_id: sent_rfc822_message_id,
            origin: "heuristic",
            dedupe_key: &key,
            ..Default::default()
        },
        now,
    )
    .map_err(db_err)
}

/// (c) For each newly arrived inbound message in a thread that has a WAITING
/// work item, propose moving that item to `needs_reply`. One proposal per
/// waiting episode (`item id + waiting_since`), so further replies in the
/// same episode and a dismissed proposal never repeat. The work item itself
/// is never changed here. Messages from the account's own addresses are not
/// inbound. Returns how many NEW suggestions were created.
pub fn propose_reply_state_changes(
    db: &Database,
    message_ids: &[String],
    now: i64,
) -> Result<usize, CoreError> {
    let mut created = 0;
    for id in message_ids {
        let Some(ctx) = db.message_context(id).map_err(db_err)? else {
            continue;
        };
        let Some(thread_id) = ctx.thread_id.as_deref() else {
            continue;
        };
        if let Some(from) = ctx.from_address.as_deref() {
            let own = db.own_addresses(&ctx.account_id).map_err(db_err)?;
            if own.contains(&from.trim().to_ascii_lowercase()) {
                continue;
            }
        }
        let waiting = db
            .list_work_items_filtered(&WorkItemFilter {
                linked_thread_id: Some(thread_id.to_string()),
                states: Some(vec!["waiting".into()]),
                ..Default::default()
            })
            .map_err(db_err)?;
        for item in waiting {
            let key = format!("reply:{}:{}", item.id, item.waiting_since.unwrap_or(0));
            let made = db
                .propose_suggestion_at(
                    &NewSuggestion {
                        kind: "state_change",
                        payload_json: r#"{"to_state":"needs_reply"}"#,
                        message_id: Some(id),
                        thread_id: Some(thread_id),
                        target_work_item_id: Some(&item.id),
                        origin: "heuristic",
                        dedupe_key: &key,
                        ..Default::default()
                    },
                    now,
                )
                .map_err(db_err)?;
            created += usize::from(made.is_some());
        }
    }
    Ok(created)
}

/// Sync hook (runs on the sync worker, after thread rebuild): resolve the
/// evidence of waiting-for-reply proposals whose Sent copy just synced
/// (`all_new`), then run producer (c) over the genuine inbox arrivals
/// (`inbox_arrivals`). Best-effort by contract: callers ignore the result.
pub fn after_sync_arrivals(
    db: &Database,
    all_new: &[String],
    inbox_arrivals: &[String],
    now: i64,
) -> Result<(), CoreError> {
    for id in all_new {
        if let Some(ctx) = db.message_context(id).map_err(db_err)? {
            if let Some(rfc) = ctx.rfc822_message_id.as_deref() {
                db.attach_sent_evidence(rfc, id, ctx.thread_id.as_deref(), now)
                    .map_err(db_err)?;
            }
        }
    }
    propose_reply_state_changes(db, inbox_arrivals, now)?;
    Ok(())
}

impl Profile {
    /// Suggestions as a JSON array of `Suggestion`, newest first. `filter_json`
    /// keys, all optional: `status`, `kind`, `thread_id`, `project_id`,
    /// `limit`; an empty string means no filter, an unknown key is an error.
    pub fn list_suggestions_json(&self, filter_json: &str) -> Result<String, CoreError> {
        let filter: SuggestionFilter = if filter_json.trim().is_empty() {
            SuggestionFilter::default()
        } else {
            serde_json::from_str(filter_json)
                .map_err(|e| CoreError::Db(format!("bad suggestion filter json: {e}")))?
        };
        to_json(&self.db.list_suggestions(&filter).map_err(db_err)?)
    }

    /// `{"proposed": n}`: suggestions still awaiting a decision.
    pub fn count_suggestions_json(&self) -> Result<String, CoreError> {
        let n = self.db.count_proposed_suggestions().map_err(db_err)?;
        to_json(&serde_json::json!({ "proposed": n }))
    }

    /// Accept a suggestion (see [`Database::accept_suggestion_at`]); returns a
    /// `SuggestionAccepted` JSON object.
    pub fn accept_suggestion_json(&self, id: &str) -> Result<String, CoreError> {
        self.accept_suggestion_json_at(id, now_epoch())
    }

    /// [`Profile::accept_suggestion_json`] with an injected clock.
    pub fn accept_suggestion_json_at(&self, id: &str, now: i64) -> Result<String, CoreError> {
        let out: SuggestionAccepted = self.db.accept_suggestion_at(id, now).map_err(db_err)?;
        to_json(&out)
    }

    /// Dismiss a suggestion; returns the `Suggestion` JSON.
    pub fn dismiss_suggestion_json(&self, id: &str) -> Result<String, CoreError> {
        self.dismiss_suggestion_json_at(id, now_epoch())
    }

    /// [`Profile::dismiss_suggestion_json`] with an injected clock.
    pub fn dismiss_suggestion_json_at(&self, id: &str, now: i64) -> Result<String, CoreError> {
        to_json(&self.db.dismiss_suggestion_at(id, now).map_err(db_err)?)
    }
}

/// Wall-clock wrappers for the hooks (the algorithms above take `now`).
pub(crate) fn propose_from_extractions_now(
    db: &Database,
    thread_id: &str,
) -> Result<usize, CoreError> {
    propose_from_extractions(db, thread_id, now_epoch())
}

pub(crate) fn after_sync_arrivals_now(
    db: &Database,
    all_new: &[String],
    inbox_arrivals: &[String],
) -> Result<(), CoreError> {
    after_sync_arrivals(db, all_new, inbox_arrivals, now_epoch())
}

pub(crate) fn propose_after_send_now(
    db: &Database,
    draft_id: i64,
    account_id: &str,
    from_email: &str,
    sent_rfc822_message_id: Option<&str>,
) -> Result<Option<Suggestion>, CoreError> {
    let Some(draft) = db.get_draft(draft_id).map_err(db_err)? else {
        return Ok(None);
    };
    let mut own = db.own_addresses(account_id).map_err(db_err)?;
    own.push(from_email.to_string());
    propose_after_send(db, &draft, &own, sent_rfc822_message_id, now_epoch())
}

#[cfg(test)]
mod tests {
    use super::*;
    use castellum_db::{EntityKind, NewThreadExtraction};
    use castellum_models::{Account, DraftAddress, Mailbox, NewDraft, NewMessage};
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    /// Temp-dir profile database with account `acc` (me@local.test), an
    /// INBOX, and message `m1` (from them) in thread `t1`.
    fn db() -> Database {
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-core-suggestions-{}-{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        for s in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), s));
        }
        let db = Database::open(&path).unwrap();
        db.upsert_account(&Account {
            id: "acc".into(),
            email: "me@local.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.upsert_mailbox(&Mailbox {
            id: "acc:INBOX".into(),
            account_id: "acc".into(),
            path: "INBOX".into(),
            name: "INBOX".into(),
            role: Some("inbox".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.upsert_mailbox(&Mailbox {
            id: "acc:Sent".into(),
            account_id: "acc".into(),
            path: "Sent".into(),
            name: "Sent".into(),
            role: Some("sent".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        msg(&db, "m1", 1, "<1@x.test>", "them@x.test", &[]);
        db.rebuild_threads("acc").unwrap();
        db
    }

    fn msg(db: &Database, id: &str, uid: i64, rfc: &str, from: &str, refs: &[&str]) {
        msg_in(db, "acc:INBOX", id, uid, rfc, from, refs);
    }

    fn msg_in(
        db: &Database,
        mailbox: &str,
        id: &str,
        uid: i64,
        rfc: &str,
        from: &str,
        refs: &[&str],
    ) {
        db.upsert_message(&NewMessage {
            id: id.into(),
            account_id: "acc".into(),
            mailbox_id: mailbox.into(),
            provider_uid: Some(uid),
            rfc822_message_id: Some(rfc.into()),
            subject: Some("S".into()),
            from_name: None,
            from_address: Some(from.into()),
            sent_at: Some(1_800_000_000 + uid),
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
            references: refs.iter().map(|r| r.to_string()).collect(),
        })
        .unwrap();
    }

    fn thread_of(db: &Database, id: &str) -> String {
        db.message_context(id).unwrap().unwrap().thread_id.unwrap()
    }

    fn draft(db: &Database, to: &[&str], irt: Option<&str>) -> Draft {
        let id = db
            .save_draft(&NewDraft {
                id: None,
                account_id: "acc".into(),
                to: to
                    .iter()
                    .map(|e| DraftAddress {
                        name: None,
                        email: (*e).into(),
                    })
                    .collect(),
                cc: vec![],
                bcc: vec![],
                subject: "Quote".into(),
                body_text: "SECRET BODY".into(),
                body_html: None,
                from_email: None,
                from_name: None,
                in_reply_to: irt.map(Into::into),
                references_header: None,
            })
            .unwrap();
        db.get_draft(id).unwrap().unwrap()
    }

    // ---- clock helpers -------------------------------------------------

    #[test]
    fn working_days_skip_weekends() {
        // 2026-10-01 is a Thursday (12:00 UTC).
        let thu = parse_iso_date("2026-10-01").unwrap() + 12 * 3600;
        // Thu + 3 working days = Tue 2026-10-06, same time of day.
        assert_eq!(
            add_working_days(thu, FOLLOW_UP_WORKING_DAYS),
            parse_iso_date("2026-10-06").unwrap() + 12 * 3600
        );
        // Monday + 3 = Thursday.
        let mon = parse_iso_date("2026-10-05").unwrap();
        assert_eq!(
            add_working_days(mon, 3),
            parse_iso_date("2026-10-08").unwrap()
        );
        // Saturday start counts from Monday: Sat + 1 = Mon.
        let sat = parse_iso_date("2026-10-03").unwrap();
        assert_eq!(
            add_working_days(sat, 1),
            parse_iso_date("2026-10-05").unwrap()
        );
        assert_eq!(add_working_days(thu, 0), thu);
    }

    #[test]
    fn iso_dates_parse_and_reject_junk() {
        assert_eq!(parse_iso_date("1970-01-02"), Some(86_400));
        assert_eq!(parse_iso_date("2026-13-01"), None);
        assert_eq!(parse_iso_date("soon"), None);
        assert_eq!(parse_iso_date("2026-10-01-5"), None);
    }

    // ---- (a) extractions -----------------------------------------------

    fn store_items(db: &Database, thread: &str, items: &[(&str, &str, Option<&str>)]) {
        let rows: Vec<NewThreadExtraction<'_>> = items
            .iter()
            .map(|(kind, text, sv)| NewThreadExtraction {
                message_id: Some("m1"),
                kind,
                text,
                structured_value: *sv,
            })
            .collect();
        db.replace_thread_extractions(thread, &rows).unwrap();
    }

    #[test]
    fn extraction_action_items_become_one_suggestion_each_and_only_once() {
        let db = db();
        let t = thread_of(&db, "m1");
        store_items(
            &db,
            &t,
            &[
                ("action_item", "Reply to Jane by Friday", None),
                ("date", "Meeting 25 Sep", Some(r#"{"date":"2026-09-25"}"#)),
                (
                    "amount",
                    "Total 5 GBP",
                    Some(r#"{"amount":5,"currency":"GBP"}"#),
                ),
                (
                    "action_item",
                    "Send the invoice",
                    Some(r#"{"date":"2026-10-09"}"#),
                ),
            ],
        );
        assert_eq!(propose_from_extractions(&db, &t, 100).unwrap(), 2);
        assert_eq!(propose_from_extractions(&db, &t, 200).unwrap(), 0);
        let all = db.list_suggestions(&Default::default()).unwrap();
        assert_eq!(all.len(), 2);
        let inv = all
            .iter()
            .find(|s| s.payload["title"] == "Send the invoice")
            .unwrap();
        assert_eq!(inv.payload["due_at"], parse_iso_date("2026-10-09").unwrap());
        assert_eq!(inv.kind, "work_item");
        assert_eq!(inv.origin, "ai_extraction");
        assert_eq!(inv.thread_id.as_deref(), Some(t.as_str()));
        assert_eq!(inv.message_id.as_deref(), Some("m1"));
        assert!(inv.extraction_id.is_some());
        let jane = all
            .iter()
            .find(|s| s.payload["title"] == "Reply to Jane by Friday")
            .unwrap();
        assert!(jane.payload["due_at"].is_null());
        // A re-extraction replaces the rows (new ids) but proposes nothing new.
        store_items(
            &db,
            &t,
            &[("action_item", "reply to  Jane by friday", None)],
        );
        assert_eq!(propose_from_extractions(&db, &t, 300).unwrap(), 0);
        // Nothing became a work item.
        assert!(db
            .list_work_items_filtered(&Default::default())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_dismissed_extraction_suggestion_is_never_re_proposed() {
        let db = db();
        let t = thread_of(&db, "m1");
        store_items(&db, &t, &[("action_item", "Book the room", None)]);
        propose_from_extractions(&db, &t, 100).unwrap();
        let s = db.list_suggestions(&Default::default()).unwrap().remove(0);
        db.dismiss_suggestion_at(&s.id, 150).unwrap();
        store_items(&db, &t, &[("action_item", "Book the room", None)]);
        assert_eq!(propose_from_extractions(&db, &t, 200).unwrap(), 0);
        assert_eq!(db.count_proposed_suggestions().unwrap(), 0);
    }

    // ---- (b) after send ------------------------------------------------

    #[test]
    fn a_sent_message_proposes_one_follow_up_three_working_days_on() {
        let db = db();
        let d = draft(&db, &["them@x.test"], Some("<1@x.test>"));
        let own = vec!["me@local.test".to_string()];
        // Thursday 2026-10-01 10:00 UTC, injected.
        let now = parse_iso_date("2026-10-01").unwrap() + 10 * 3600;
        let s = propose_after_send(&db, &d, &own, Some("<sent1@local.test>"), now)
            .unwrap()
            .expect("proposed");
        assert_eq!(s.kind, "waiting_for_reply");
        assert_eq!(s.origin, "heuristic");
        assert_eq!(s.payload["title"], "Quote");
        assert_eq!(
            s.payload["due_at"],
            parse_iso_date("2026-10-06").unwrap() + 10 * 3600
        );
        // A reply resolves its thread and message straight away.
        assert_eq!(s.thread_id.as_deref(), Some(thread_of(&db, "m1").as_str()));
        assert_eq!(s.message_id.as_deref(), Some("m1"));
        assert_eq!(s.rfc822_message_id.as_deref(), Some("<sent1@local.test>"));
        // No body text anywhere in the row.
        assert!(!format!("{s:?}").contains("SECRET BODY"));
        // Idempotent per draft.
        assert!(
            propose_after_send(&db, &d, &own, Some("<sent1@local.test>"), now + 5)
                .unwrap()
                .is_none()
        );
        assert_eq!(db.list_suggestions(&Default::default()).unwrap().len(), 1);
        // Nothing became a work item.
        assert!(db
            .list_work_items_filtered(&Default::default())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_message_sent_only_to_yourself_proposes_nothing() {
        let db = db();
        let own = vec!["me@local.test".to_string(), "alias@local.test".to_string()];
        let to_self = draft(&db, &["Me@Local.test"], None);
        assert!(propose_after_send(&db, &to_self, &own, None, 1)
            .unwrap()
            .is_none());
        let to_self_and_alias = draft(&db, &["me@local.test", "alias@local.test"], None);
        assert!(propose_after_send(&db, &to_self_and_alias, &own, None, 1)
            .unwrap()
            .is_none());
        // One outside recipient is enough.
        let mixed = draft(&db, &["me@local.test", "them@x.test"], None);
        assert!(propose_after_send(&db, &mixed, &own, None, 1)
            .unwrap()
            .is_some());
        assert_eq!(db.list_suggestions(&Default::default()).unwrap().len(), 1);
    }

    #[test]
    fn the_send_hook_resolves_late_evidence_when_the_sent_copy_syncs() {
        let db = db();
        let d = draft(&db, &["them@x.test"], None);
        let s = propose_after_send(&db, &d, &[], Some("<sent9@local.test>"), 10)
            .unwrap()
            .unwrap();
        assert!(s.thread_id.is_none());
        // A forged copy in the INBOX (From = me, same Message-ID) is ignored.
        msg(&db, "m9", 9, "<sent9@local.test>", "me@local.test", &[]);
        db.rebuild_threads("acc").unwrap();
        after_sync_arrivals(&db, &["m9".to_string()], &[], 15).unwrap();
        assert!(db
            .get_suggestion(&s.id)
            .unwrap()
            .unwrap()
            .message_id
            .is_none());
        // The Sent copy arrives later (as message m2 in thread of its own).
        msg_in(
            &db,
            "acc:Sent",
            "m2",
            2,
            "<sent9@local.test>",
            "me@local.test",
            &[],
        );
        db.rebuild_threads("acc").unwrap();
        after_sync_arrivals(&db, &["m2".to_string()], &[], 20).unwrap();
        let got = db.get_suggestion(&s.id).unwrap().unwrap();
        assert_eq!(got.message_id.as_deref(), Some("m2"));
        assert_eq!(
            got.thread_id.as_deref(),
            Some(thread_of(&db, "m2").as_str())
        );
    }

    // ---- (c) reply to a waiting item -------------------------------------

    fn waiting_item(db: &Database, thread: &str, since: i64) -> String {
        let w = db
            .upsert_work_item_at(
                castellum_db::WorkItemUpsert {
                    title: "Chase quote",
                    state: "waiting",
                    waiting_since: Some(since),
                    ..Default::default()
                },
                since,
            )
            .unwrap();
        db.link_work_item(&w.id, EntityKind::Thread, thread)
            .unwrap();
        w.id
    }

    #[test]
    fn an_inbound_reply_proposes_needs_reply_and_leaves_the_item_alone() {
        let db = db();
        let t = thread_of(&db, "m1");
        let w = waiting_item(&db, &t, 50);
        let before = db.get_work_item(&w).unwrap().unwrap();
        msg(&db, "m3", 3, "<3@x.test>", "them@x.test", &["<1@x.test>"]);
        db.rebuild_threads("acc").unwrap();
        assert_eq!(thread_of(&db, "m3"), t);
        assert_eq!(
            propose_reply_state_changes(&db, &["m3".into()], 100).unwrap(),
            1
        );
        // Idempotent, including for a second reply in the same episode.
        assert_eq!(
            propose_reply_state_changes(&db, &["m3".into()], 101).unwrap(),
            0
        );
        msg(&db, "m4", 4, "<4@x.test>", "them@x.test", &["<1@x.test>"]);
        db.rebuild_threads("acc").unwrap();
        assert_eq!(
            propose_reply_state_changes(&db, &["m4".into()], 102).unwrap(),
            0
        );
        let all = db.list_suggestions(&Default::default()).unwrap();
        assert_eq!(all.len(), 1);
        let s = &all[0];
        assert_eq!(s.kind, "state_change");
        assert_eq!(s.payload["to_state"], "needs_reply");
        assert_eq!(s.target_work_item_id.as_deref(), Some(w.as_str()));
        assert_eq!(s.message_id.as_deref(), Some("m3"));
        assert_eq!(s.thread_id.as_deref(), Some(t.as_str()));
        // The work item is exactly as it was.
        assert_eq!(db.get_work_item(&w).unwrap().unwrap(), before);
        // Accepting is the only thing that changes it.
        let moved = db
            .accept_suggestion_at(&s.id, 200)
            .unwrap()
            .work_item
            .unwrap();
        assert_eq!(
            (moved.state.as_str(), moved.waiting_since),
            ("needs_reply", None)
        );
    }

    #[test]
    fn replies_that_do_not_qualify_propose_nothing() {
        let db = db();
        let t = thread_of(&db, "m1");
        // No waiting item in the thread: nothing.
        assert_eq!(
            propose_reply_state_changes(&db, &["m1".into()], 1).unwrap(),
            0
        );
        let w = waiting_item(&db, &t, 50);
        // My own message (e.g. a Sent copy) is not a reply.
        msg(&db, "m5", 5, "<5@x.test>", "ME@local.test", &["<1@x.test>"]);
        db.rebuild_threads("acc").unwrap();
        assert_eq!(
            propose_reply_state_changes(&db, &["m5".into()], 1).unwrap(),
            0
        );
        // An item that is not waiting any more: nothing.
        db.upsert_work_item_at(
            castellum_db::WorkItemUpsert {
                id: Some(&w),
                title: "Chase quote",
                state: "done",
                ..Default::default()
            },
            60,
        )
        .unwrap();
        assert_eq!(
            propose_reply_state_changes(&db, &["m1".into()], 1).unwrap(),
            0
        );
        // Unknown message id: skipped, not an error.
        assert_eq!(
            propose_reply_state_changes(&db, &["ghost".into()], 1).unwrap(),
            0
        );
        assert_eq!(db.count_proposed_suggestions().unwrap(), 0);
    }

    // ---- profile JSON surface ------------------------------------------

    #[test]
    fn json_list_count_accept_dismiss_round_trip() {
        let dir = castellum_testutil::scratch_root().join(format!(
            "castellum-core-suggestions-profile-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = crate::open_profile(&dir).expect("profile");
        p.db.propose_suggestion_at(
            &NewSuggestion {
                kind: "work_item",
                payload_json: r#"{"title":"A"}"#,
                origin: "heuristic",
                dedupe_key: "a",
                ..Default::default()
            },
            1,
        )
        .unwrap();
        p.db.propose_suggestion_at(
            &NewSuggestion {
                kind: "work_item",
                payload_json: r#"{"title":"B"}"#,
                origin: "heuristic",
                dedupe_key: "b",
                ..Default::default()
            },
            2,
        )
        .unwrap();
        assert_eq!(p.count_suggestions_json().unwrap(), r#"{"proposed":2}"#);
        let list: serde_json::Value =
            serde_json::from_str(&p.list_suggestions_json(r#"{"status":"proposed"}"#).unwrap())
                .unwrap();
        assert_eq!(list[0]["payload"]["title"], "B");
        let a = list[1]["id"].as_str().unwrap().to_string();
        let b = list[0]["id"].as_str().unwrap().to_string();
        let acc: serde_json::Value =
            serde_json::from_str(&p.accept_suggestion_json_at(&a, 9).unwrap()).unwrap();
        assert_eq!(acc["work_item"]["title"], "A");
        assert_eq!(acc["suggestion"]["status"], "accepted");
        let dis: serde_json::Value =
            serde_json::from_str(&p.dismiss_suggestion_json_at(&b, 9).unwrap()).unwrap();
        assert_eq!(dis["status"], "dismissed");
        assert_eq!(p.count_suggestions_json().unwrap(), r#"{"proposed":0}"#);
        assert!(p.accept_suggestion_json_at(&b, 10).is_err());
        assert!(p.list_suggestions_json(r#"{"bogus":1}"#).is_err());
        assert!(p.list_suggestions_json("").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
