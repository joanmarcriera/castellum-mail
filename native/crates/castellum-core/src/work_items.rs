// Castellum - work-item JSON wrappers (Active Work backend).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Thin serialization layer over the work_items repository, mirroring
// projects.rs, plus the Active Work state-transition rules. Work items are
// NEVER created automatically from mail — every write comes through these
// explicit APIs, and title/notes are user-owned: this layer stores exactly
// what the caller sent and never rewrites them. Relationships to messages/
// threads/contacts go through the shared link model (links.rs); the helpers
// here are thin, validated wrappers over it.
//
// Transition rules (all take an injectable clock, `*_at(.., now)`):
// - entering `waiting` sets `waiting_since` if it is null; leaving it clears it
// - entering `done` sets `completed_at`; leaving it clears it
// - `delegated_to_contact_id` is only meaningful in `delegated` (an optional
//   contact); leaving `delegated` clears it, and supplying one in any other
//   state is an error
// - omitted `due_at`/`delegated_to_contact_id` keep their stored value
//   (old callers that never send them keep working); an explicit `null` clears

use crate::{CoreError, Profile};
use castellum_db::{EntityKind, WorkItem, WorkItemFilter, WorkItemUpsert};
use serde::{Deserialize, Deserializer};

fn to_json<T: serde::Serialize>(value: &T) -> Result<String, CoreError> {
    serde_json::to_string(value).map_err(|_| CoreError::Io)
}

fn db_io(e: castellum_db::DbError) -> CoreError {
    CoreError::Db(e.to_string())
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Distinguishes an absent key (`None`: keep) from an explicit `null`
/// (`Some(None)`: clear) from a value (`Some(Some(v))`: set).
fn keep_or_set<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(Option::deserialize(d)?))
}

/// Upsert request sent from the UI.
#[derive(Deserialize)]
struct UpsertWorkItemRequest {
    #[serde(default)]
    id: Option<String>,
    /// Absent or null = an unassigned item.
    #[serde(default)]
    project_id: Option<String>,
    title: String,
    /// Absent: keep the stored notes on update, empty on create.
    #[serde(default)]
    notes: Option<String>,
    #[serde(default = "default_state")]
    state: String,
    #[serde(default)]
    position: i64,
    #[serde(default, deserialize_with = "keep_or_set")]
    due_at: Option<Option<i64>>,
    #[serde(default, deserialize_with = "keep_or_set")]
    delegated_to_contact_id: Option<Option<i64>>,
}

fn default_state() -> String {
    "needs_reply".into()
}

/// The two derived timestamps for an item moving from `prev` (None = being
/// created) into `new_state` at `now`: `(waiting_since, completed_at)`.
pub(crate) fn resolve_transition(
    prev: Option<&WorkItem>,
    new_state: &str,
    now: i64,
) -> (Option<i64>, Option<i64>) {
    let waiting_since = if new_state == "waiting" {
        // Keep the original moment if the item is already waiting.
        prev.and_then(|p| p.waiting_since).or(Some(now))
    } else {
        None
    };
    let completed_at = if new_state == "done" {
        match prev {
            Some(p) if p.state == "done" && p.completed_at.is_some() => p.completed_at,
            _ => Some(now),
        }
    } else {
        None
    };
    (waiting_since, completed_at)
}

/// Request body for `work_items_for`: `{kind: message|thread|contact, id}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkItemsForRequest {
    kind: String,
    id: String,
}

/// Request body for `record_work_item_provenance`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordProvenanceRequest {
    work_item_id: String,
    source_kind: String,
    source_id: Option<String>,
}

/// Request body for `work_items_from_source`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FromSourceRequest {
    source_kind: String,
    source_id: String,
}

fn link_target_kind(kind: &str) -> Result<EntityKind, CoreError> {
    EntityKind::parse(kind).ok_or_else(|| CoreError::Db(format!("unknown endpoint kind {kind:?}")))
}

impl Profile {
    /// Create or update a work item from JSON; returns the stored item.
    /// Shape: `{id?, project_id?, title, notes?, state?, position?, due_at?,
    /// delegated_to_contact_id?}` (state defaults to 'needs_reply').
    pub fn upsert_work_item_json(&self, json: &str) -> Result<String, CoreError> {
        self.upsert_work_item_json_at(json, now_epoch())
    }

    /// [`Profile::upsert_work_item_json`] with an injected clock (epoch
    /// seconds) used for `created_at`/`updated_at` and the transition rules.
    pub fn upsert_work_item_json_at(&self, json: &str, now: i64) -> Result<String, CoreError> {
        let req: UpsertWorkItemRequest = serde_json::from_str(json)
            .map_err(|e| CoreError::Db(format!("bad upsert_work_item json: {e}")))?;
        let existing_id = req.id.as_deref().filter(|s| !s.is_empty());
        let prev = match existing_id {
            Some(id) => Some(
                self.db
                    .get_work_item(id)
                    .map_err(db_io)?
                    .ok_or_else(|| CoreError::Db(format!("work item {id} not found")))?,
            ),
            None => None,
        };
        let project_id = req
            .project_id
            .as_deref()
            .or_else(|| prev.as_ref().and_then(|p| p.project_id.as_deref()));
        let (waiting_since, completed_at) = resolve_transition(prev.as_ref(), &req.state, now);
        let due_at = match req.due_at {
            Some(explicit) => explicit,
            None => prev.as_ref().and_then(|p| p.due_at),
        };
        let delegate = if req.state == "delegated" {
            match req.delegated_to_contact_id {
                Some(explicit) => explicit,
                // Keep the stored delegate only while staying delegated.
                None => prev
                    .as_ref()
                    .filter(|p| p.state == "delegated")
                    .and_then(|p| p.delegated_to_contact_id),
            }
        } else if matches!(req.delegated_to_contact_id, Some(Some(_))) {
            return Err(CoreError::Db(
                "delegated_to_contact_id is only valid in the 'delegated' state".into(),
            ));
        } else {
            None
        };
        let notes = match (&req.notes, &prev) {
            (Some(n), _) => n.as_str(),
            (None, Some(p)) => p.notes.as_str(),
            (None, None) => "",
        };
        to_json(
            &self
                .db
                .upsert_work_item_at(
                    WorkItemUpsert {
                        id: req.id.as_deref(),
                        project_id,
                        title: &req.title,
                        notes,
                        state: &req.state,
                        position: req.position,
                        due_at,
                        waiting_since,
                        completed_at,
                        delegated_to_contact_id: delegate,
                    },
                    now,
                )
                .map_err(db_io)?,
        )
    }

    /// A project's work items in deterministic order as a JSON array.
    pub fn list_work_items_json(&self, project_id: &str) -> Result<String, CoreError> {
        to_json(&self.db.list_work_items(project_id).map_err(db_io)?)
    }

    /// Filtered work items as a JSON array, ordered by due date (undated
    /// last), position, id. `filter_json` keys: project_id, states,
    /// due_before, due_after, overdue, contact_id, linked_message_id,
    /// linked_thread_id, unassigned, due (today/this_week/overdue list),
    /// waiting_on_me, waiting_on_others. Unknown keys are an error.
    pub fn list_work_items_filtered_json(&self, filter_json: &str) -> Result<String, CoreError> {
        self.list_work_items_filtered_json_at(filter_json, now_epoch())
    }

    /// [`Profile::list_work_items_filtered_json`] with an injected clock for
    /// `overdue`.
    pub fn list_work_items_filtered_json_at(
        &self,
        filter_json: &str,
        now: i64,
    ) -> Result<String, CoreError> {
        let filter: WorkItemFilter = serde_json::from_str(filter_json)
            .map_err(|e| CoreError::Db(format!("bad work-item filter json: {e}")))?;
        to_json(
            &self
                .db
                .list_work_items_filtered_at(&filter, now)
                .map_err(db_io)?,
        )
    }

    /// Reverse lookup: the work items of a message, thread or contact.
    /// `request_json` = `{kind, id}`.
    pub fn work_items_for_json(&self, request_json: &str) -> Result<String, CoreError> {
        let req: WorkItemsForRequest = serde_json::from_str(request_json)
            .map_err(|e| CoreError::Db(format!("bad work_items_for json: {e}")))?;
        to_json(
            &self
                .db
                .work_items_for(link_target_kind(&req.kind)?, &req.id)
                .map_err(db_io)?,
        )
    }

    /// Link a work item to a message, thread or contact (manual, idempotent);
    /// returns the stored LinkRecord JSON.
    pub fn link_work_item_json(
        &self,
        work_item_id: &str,
        kind: &str,
        target_id: &str,
    ) -> Result<String, CoreError> {
        to_json(
            &self
                .db
                .link_work_item(work_item_id, link_target_kind(kind)?, target_id)
                .map_err(db_io)?,
        )
    }

    /// A work item's provenance (message/suggestion/manual sources) as a JSON
    /// array of `{work_item_id, source_kind, source_id, created_at}`, oldest
    /// first. Unknown id -> `[]`.
    pub fn list_work_item_provenance_json(&self, work_item_id: &str) -> Result<String, CoreError> {
        to_json(
            &self
                .db
                .list_work_item_provenance(work_item_id)
                .map_err(db_io)?,
        )
    }

    /// Record a manual provenance row. `request_json` =
    /// `{work_item_id, source_kind: "manual", source_id?}` (any other kind is
    /// rejected; message/suggestion rows come from the accept/link paths);
    /// idempotent; returns the stored row. Never touches the item's notes.
    pub fn record_work_item_provenance_json(
        &self,
        request_json: &str,
    ) -> Result<String, CoreError> {
        let req: RecordProvenanceRequest = serde_json::from_str(request_json)
            .map_err(|e| CoreError::Db(format!("bad provenance json: {e}")))?;
        to_json(
            &self
                .db
                .record_work_item_provenance(
                    &req.work_item_id,
                    &req.source_kind,
                    req.source_id.as_deref().unwrap_or(""),
                )
                .map_err(db_io)?,
        )
    }

    /// Reverse lookup: JSON array of work-item ids that came from a source.
    /// `request_json` = `{source_kind, source_id}`.
    pub fn work_items_from_source_json(&self, request_json: &str) -> Result<String, CoreError> {
        let req: FromSourceRequest = serde_json::from_str(request_json)
            .map_err(|e| CoreError::Db(format!("bad from_source json: {e}")))?;
        to_json(
            &self
                .db
                .work_items_from_source(&req.source_kind, &req.source_id)
                .map_err(db_io)?,
        )
    }

    /// Remove such a link; true if a row was removed.
    pub fn unlink_work_item(
        &self,
        work_item_id: &str,
        kind: &str,
        target_id: &str,
    ) -> Result<bool, CoreError> {
        self.db
            .unlink_work_item(work_item_id, link_target_kind(kind)?, target_id)
            .map_err(db_io)
    }

    /// Permanently delete a work item (its link rows cascade).
    pub fn delete_work_item(&self, id: &str) -> Result<(), CoreError> {
        self.db.delete_work_item(id).map_err(db_io)
    }
}

#[cfg(test)]
mod tests {
    use crate::open_profile;
    use castellum_models::{Account, Mailbox, NewMessage};

    fn profile(tag: &str) -> crate::Profile {
        let dir = castellum_testutil::scratch_root().join(format!(
            "castellum-work-items-core-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        open_profile(&dir).unwrap()
    }

    fn parse(json: &str) -> serde_json::Value {
        serde_json::from_str(json).unwrap()
    }

    /// Seed one real message so link-model endpoint validation is satisfied.
    fn seed_message(p: &crate::Profile, id: &str) {
        p.db.upsert_account(&Account {
            id: "acc".into(),
            email: "me@local.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        p.db.upsert_mailbox(&Mailbox {
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
        p.db.upsert_message(&NewMessage {
            id: id.into(),
            account_id: "acc".into(),
            mailbox_id: "acc:INBOX".into(),
            provider_uid: Some(1),
            rfc822_message_id: Some("<1@local.test>".into()),
            subject: Some("S".into()),
            from_name: None,
            from_address: Some("s@local.test".into()),
            sent_at: Some(1_800_000_000),
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
            references: Vec::new(),
        })
        .unwrap();
    }

    #[test]
    fn provenance_json_wrappers_record_list_and_reverse_lookup() {
        let p = profile("provenance");
        seed_message(&p, "acc:INBOX:5");
        let created = parse(
            &p.upsert_work_item_json(r#"{"title":"T","notes":"mine","state":"needs_reply"}"#)
                .unwrap(),
        );
        let wid = created["id"].as_str().unwrap().to_string();
        p.link_work_item_json(&wid, "message", "acc:INBOX:5")
            .unwrap();
        p.record_work_item_provenance_json(&format!(
            r#"{{"work_item_id":"{wid}","source_kind":"manual"}}"#
        ))
        .unwrap();
        let listed = parse(&p.list_work_item_provenance_json(&wid).unwrap());
        let kinds: Vec<&str> = listed
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["source_kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds.len(), 2);
        assert!(kinds.contains(&"message") && kinds.contains(&"manual"));
        let from = parse(
            &p.work_items_from_source_json(
                r#"{"source_kind":"message","source_id":"acc:INBOX:5"}"#,
            )
            .unwrap(),
        );
        assert_eq!(from, parse(&format!(r#"["{wid}"]"#)));
        assert!(p
            .record_work_item_provenance_json(r#"{"work_item_id":"x","source_kind":"bogus"}"#)
            .is_err());
        assert!(p
            .record_work_item_provenance_json(&format!(
                r#"{{"work_item_id":"{wid}","source_kind":"message","source_id":"acc:INBOX:5"}}"#
            ))
            .is_err());
        assert!(p.record_work_item_provenance_json("{bad").is_err());
        // Notes untouched.
        let w = parse(&p.list_work_items_filtered_json("{}").unwrap());
        assert_eq!(w[0]["notes"], "mine");
    }

    #[test]
    fn work_item_json_round_trip_with_link_to_message() {
        let p = profile("roundtrip");
        seed_message(&p, "acc:INBOX:5");
        let project = parse(
            &p.upsert_project_json(r#"{"name":"P","status":"active"}"#)
                .unwrap(),
        );
        let pid = project["id"].as_str().unwrap().to_string();

        let created = parse(
            &p.upsert_work_item_json(&format!(
                r#"{{"project_id":"{pid}","title":"Chase reply","state":"waiting"}}"#
            ))
            .unwrap(),
        );
        assert_eq!(created["state"], "waiting");
        let wid = created["id"].as_str().unwrap().to_string();

        // Update in place: notes are user-owned and round-trip exactly.
        let updated = parse(
            &p.upsert_work_item_json(&format!(
                r#"{{"id":"{wid}","project_id":"{pid}","title":"Chase reply",
                    "notes":"asked on Monday","state":"delegated","position":2}}"#
            ))
            .unwrap(),
        );
        assert_eq!(updated["notes"], "asked on Monday");
        assert_eq!(updated["state"], "delegated");

        // Link to a message through the SHARED link model and read it back.
        p.upsert_link_json(&format!(
            r#"{{"src_kind":"work_item","src_id":"{wid}",
                "dst_kind":"message","dst_id":"acc:INBOX:5","relation":"tracks"}}"#
        ))
        .unwrap();
        let links = parse(&p.links_for_json("work_item", &wid).unwrap());
        assert_eq!(links.as_array().unwrap().len(), 1);

        let listed = parse(&p.list_work_items_json(&pid).unwrap());
        assert_eq!(listed.as_array().unwrap().len(), 1);

        p.delete_work_item(&wid).unwrap();
        assert_eq!(parse(&p.list_work_items_json(&pid).unwrap()), parse("[]"));
        assert_eq!(
            parse(&p.links_for_json("work_item", &wid).unwrap()),
            parse("[]"),
            "deleting the item cascades its link rows"
        );
    }

    #[test]
    fn invalid_state_and_bad_json_are_rejected() {
        let p = profile("bad");
        let project = parse(
            &p.upsert_project_json(r#"{"name":"P","status":"active"}"#)
                .unwrap(),
        );
        let pid = project["id"].as_str().unwrap();
        assert!(p
            .upsert_work_item_json(&format!(
                r#"{{"project_id":"{pid}","title":"T","state":"doing"}}"#
            ))
            .is_err());
        assert!(p.upsert_work_item_json("not json").is_err());
    }

    // ---- AW1: transition rules (injected clock) ---------------------------

    const T0: i64 = 1_800_000_000;

    fn setup(tag: &str) -> (crate::Profile, String) {
        let p = profile(tag);
        let project = parse(
            &p.upsert_project_json(r#"{"name":"P","status":"active"}"#)
                .unwrap(),
        );
        let pid = project["id"].as_str().unwrap().to_string();
        (p, pid)
    }

    /// Upsert at an injected time; `extra` is spliced into the JSON object.
    fn put(
        p: &crate::Profile,
        pid: &str,
        id: Option<&str>,
        state: &str,
        extra: &str,
        now: i64,
    ) -> serde_json::Value {
        let id_part = id.map(|i| format!(r#""id":"{i}","#)).unwrap_or_default();
        parse(
            &p.upsert_work_item_json_at(
                &format!(
                    r#"{{{id_part}"project_id":"{pid}","title":"T","state":"{state}"{extra}}}"#
                ),
                now,
            )
            .unwrap(),
        )
    }

    #[test]
    fn entering_waiting_sets_waiting_since_from_the_injected_clock() {
        let (p, pid) = setup("enter-waiting");
        let a = put(&p, &pid, None, "needs_reply", "", T0);
        assert!(a["waiting_since"].is_null());
        let b = put(&p, &pid, a["id"].as_str(), "waiting", "", T0 + 100);
        assert_eq!(b["waiting_since"], T0 + 100);
        assert_eq!(b["updated_at"], T0 + 100);
    }

    #[test]
    fn creating_an_item_directly_in_waiting_sets_waiting_since() {
        let (p, pid) = setup("create-waiting");
        let a = put(&p, &pid, None, "waiting", "", T0);
        assert_eq!(a["waiting_since"], T0);
    }

    #[test]
    fn staying_in_waiting_keeps_the_original_waiting_since() {
        let (p, pid) = setup("stay-waiting");
        let a = put(&p, &pid, None, "waiting", "", T0);
        let b = put(&p, &pid, a["id"].as_str(), "waiting", "", T0 + 500);
        assert_eq!(b["waiting_since"], T0, "an edit must not reset the clock");
    }

    #[test]
    fn leaving_waiting_clears_waiting_since() {
        let (p, pid) = setup("leave-waiting");
        let a = put(&p, &pid, None, "waiting", "", T0);
        let b = put(&p, &pid, a["id"].as_str(), "blocked", "", T0 + 10);
        assert!(b["waiting_since"].is_null());
        // Re-entering waiting starts a fresh clock.
        let c = put(&p, &pid, a["id"].as_str(), "waiting", "", T0 + 20);
        assert_eq!(c["waiting_since"], T0 + 20);
    }

    #[test]
    fn entering_done_sets_completed_at_and_leaving_done_clears_it() {
        let (p, pid) = setup("done");
        let a = put(&p, &pid, None, "needs_reply", "", T0);
        assert!(a["completed_at"].is_null());
        let done = put(&p, &pid, a["id"].as_str(), "done", "", T0 + 60);
        assert_eq!(done["completed_at"], T0 + 60);
        let still = put(&p, &pid, a["id"].as_str(), "done", "", T0 + 90);
        assert_eq!(
            still["completed_at"],
            T0 + 60,
            "editing a done item keeps it"
        );
        let reopened = put(&p, &pid, a["id"].as_str(), "needs_reply", "", T0 + 120);
        assert!(reopened["completed_at"].is_null());
    }

    #[test]
    fn waiting_straight_to_done_clears_waiting_and_sets_completed() {
        let (p, pid) = setup("wait-done");
        let a = put(&p, &pid, None, "waiting", "", T0);
        let b = put(&p, &pid, a["id"].as_str(), "done", "", T0 + 5);
        assert!(b["waiting_since"].is_null());
        assert_eq!(b["completed_at"], T0 + 5);
    }

    #[test]
    fn delegated_accepts_an_optional_contact() {
        let (p, pid) = setup("delegated");
        let c = p.db.create_contact("d@local.test", None).unwrap();
        let without = put(&p, &pid, None, "delegated", "", T0);
        assert!(without["delegated_to_contact_id"].is_null(), "optional");
        let with = put(
            &p,
            &pid,
            without["id"].as_str(),
            "delegated",
            &format!(r#","delegated_to_contact_id":{}"#, c.id),
            T0 + 1,
        );
        assert_eq!(with["delegated_to_contact_id"], c.id);
        // Staying delegated with the key omitted keeps the contact.
        let kept = put(&p, &pid, without["id"].as_str(), "delegated", "", T0 + 2);
        assert_eq!(kept["delegated_to_contact_id"], c.id);
        // Explicit null clears it.
        let cleared = put(
            &p,
            &pid,
            without["id"].as_str(),
            "delegated",
            r#","delegated_to_contact_id":null"#,
            T0 + 3,
        );
        assert!(cleared["delegated_to_contact_id"].is_null());
    }

    #[test]
    fn delegate_contact_is_refused_outside_delegated_and_cleared_on_leaving() {
        let (p, pid) = setup("delegate-rules");
        let c = p.db.create_contact("d@local.test", None).unwrap();
        let extra = format!(r#","delegated_to_contact_id":{}"#, c.id);
        let err = p
            .upsert_work_item_json_at(
                &format!(r#"{{"project_id":"{pid}","title":"T","state":"waiting"{extra}}}"#),
                T0,
            )
            .unwrap_err();
        assert!(err.to_string().contains("delegated"), "{err}");
        let d = put(&p, &pid, None, "delegated", &extra, T0);
        assert_eq!(d["delegated_to_contact_id"], c.id);
        let back = put(&p, &pid, d["id"].as_str(), "blocked", "", T0 + 1);
        assert!(back["delegated_to_contact_id"].is_null());
        // A contact that does not exist is refused.
        assert!(p
            .upsert_work_item_json_at(
                &format!(
                    r#"{{"project_id":"{pid}","title":"T","state":"delegated","delegated_to_contact_id":424242}}"#
                ),
                T0,
            )
            .is_err());
    }

    #[test]
    fn title_and_notes_are_never_rewritten_by_transitions() {
        let (p, pid) = setup("user-owned");
        let a = parse(
            &p.upsert_work_item_json_at(
                &format!(
                    r#"{{"project_id":"{pid}","title":"  Mine, as typed ","notes":"line1\nline2","state":"needs_reply"}}"#
                ),
                T0,
            )
            .unwrap(),
        );
        let id = a["id"].as_str().unwrap();
        for (i, state) in ["waiting", "done", "someday", "blocked", "waiting"]
            .iter()
            .enumerate()
        {
            // Notes omitted on purpose: the stored text must survive.
            let r = parse(
                &p.upsert_work_item_json_at(
                    &format!(
                        r#"{{"id":"{id}","project_id":"{pid}","title":"  Mine, as typed ","state":"{state}"}}"#
                    ),
                    T0 + i as i64,
                )
                .unwrap(),
            );
            assert_eq!(r["title"], "  Mine, as typed ");
            assert_eq!(r["notes"], "line1\nline2");
        }
    }

    #[test]
    fn omitted_due_at_is_kept_and_explicit_null_clears_it() {
        let (p, pid) = setup("due-keep");
        let a = put(&p, &pid, None, "waiting", r#","due_at":12345"#, T0);
        assert_eq!(a["due_at"], 12345);
        // An old caller that has never heard of due_at.
        let kept = put(&p, &pid, a["id"].as_str(), "waiting", "", T0 + 1);
        assert_eq!(kept["due_at"], 12345);
        let cleared = put(
            &p,
            &pid,
            a["id"].as_str(),
            "waiting",
            r#","due_at":null"#,
            T0 + 2,
        );
        assert!(cleared["due_at"].is_null());
    }

    #[test]
    fn nothing_is_created_automatically_from_mail() {
        let (p, _pid) = setup("no-auto");
        seed_message(&p, "acc:INBOX:7");
        let all = parse(&p.list_work_items_filtered_json_at("{}", T0).unwrap());
        assert!(all.as_array().unwrap().is_empty());
    }

    #[test]
    fn old_shape_upsert_json_still_works_and_returns_the_new_fields_as_null() {
        let (p, pid) = setup("old-shape");
        let r = parse(
            &p.upsert_work_item_json(&format!(r#"{{"project_id":"{pid}","title":"Old caller"}}"#))
                .unwrap(),
        );
        assert_eq!(r["state"], "needs_reply");
        for k in [
            "due_at",
            "waiting_since",
            "completed_at",
            "delegated_to_contact_id",
        ] {
            assert!(r[k].is_null(), "{k}");
        }
    }

    #[test]
    fn filtered_json_rejects_unknown_keys_and_orders_by_due() {
        let (p, pid) = setup("filtered-json");
        put(&p, &pid, None, "waiting", "", T0);
        let late = put(&p, &pid, None, "waiting", r#","due_at":50"#, T0);
        let err = p
            .list_work_items_filtered_json_at(r#"{"projectid":"x"}"#, T0)
            .unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
        assert!(p.list_work_items_filtered_json_at("nope", T0).is_err());
        let listed = parse(
            &p.list_work_items_filtered_json_at(r#"{"overdue":true}"#, T0)
                .unwrap(),
        );
        assert_eq!(listed[0]["id"], late["id"]);
        assert_eq!(listed.as_array().unwrap().len(), 1);
    }

    #[test]
    fn link_unlink_and_work_items_for_through_core() {
        let (p, pid) = setup("links");
        seed_message(&p, "acc:INBOX:7");
        let w = put(&p, &pid, None, "waiting", "", T0);
        let wid = w["id"].as_str().unwrap();
        let l = parse(
            &p.link_work_item_json(wid, "message", "acc:INBOX:7")
                .unwrap(),
        );
        assert_eq!(l["origin"], "manual");
        let again = parse(
            &p.link_work_item_json(wid, "message", "acc:INBOX:7")
                .unwrap(),
        );
        assert_eq!(l["id"], again["id"]);
        let got = parse(
            &p.work_items_for_json(r#"{"kind":"message","id":"acc:INBOX:7"}"#)
                .unwrap(),
        );
        assert_eq!(got[0]["id"], wid);
        // Hostile: wrong kinds, unknown request keys, missing endpoints.
        assert!(p.link_work_item_json(wid, "project", &pid).is_err());
        assert!(p.link_work_item_json(wid, "bogus", "x").is_err());
        assert!(p
            .link_work_item_json("nope", "message", "acc:INBOX:7")
            .is_err());
        assert!(p
            .work_items_for_json(r#"{"kind":"message","id":"x","extra":1}"#)
            .is_err());
        assert!(p
            .work_items_for_json(r#"{"kind":"project","id":"x"}"#)
            .is_err());
        assert!(p.unlink_work_item(wid, "message", "acc:INBOX:7").unwrap());
    }

    #[test]
    fn resolve_transition_is_pure() {
        use super::resolve_transition;
        assert_eq!(resolve_transition(None, "needs_reply", 5), (None, None));
        assert_eq!(resolve_transition(None, "waiting", 5), (Some(5), None));
        assert_eq!(resolve_transition(None, "done", 5), (None, Some(5)));
    }
}
