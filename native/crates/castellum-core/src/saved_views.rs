// Castellum - saved Active Work view JSON wrappers (AW3).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Thin serialization layer over castellum-db's saved_views and
// unassigned_mail repositories, mirroring work_items.rs. Every filter that
// arrives here is validated by the db layer (the AW1 filter schema, unknown
// keys rejected), so a bad filter never reaches the table. Counts and view
// rows go through the same evaluator as `list_work_items_filtered`.
//
// Requests (all JSON objects):
//   create   {name, filter}                 filter = a WorkItemFilter object
//   update   {id, name?, filter?, hidden?}  built-ins accept only `hidden`
//   reorder  ["id", ...]                    listed ids first, rest keep order
//   unassigned mail  {limit?, offset?}      defaults 50 / 0

use crate::{CoreError, Profile};
use serde::Deserialize;

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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateRequest {
    name: String,
    #[serde(default = "empty_filter")]
    filter: serde_json::Value,
}

fn empty_filter() -> serde_json::Value {
    serde_json::json!({})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateRequest {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    filter: Option<serde_json::Value>,
    #[serde(default)]
    hidden: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PageRequest {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
}

fn default_limit() -> i64 {
    50
}

fn bad(what: &str, e: serde_json::Error) -> CoreError {
    CoreError::Db(format!("bad {what} json: {e}"))
}

impl Profile {
    /// Saved views in display order as a JSON array (SavedView). Seeds the
    /// four built-ins on first use. Hidden views only if `include_hidden`.
    pub fn list_saved_views_json(&self, include_hidden: bool) -> Result<String, CoreError> {
        to_json(&self.db.list_saved_views(include_hidden).map_err(db_io)?)
    }

    /// Create a custom view; returns the stored SavedView.
    pub fn create_saved_view_json(&self, request_json: &str) -> Result<String, CoreError> {
        let req: CreateRequest =
            serde_json::from_str(request_json).map_err(|e| bad("create_saved_view", e))?;
        let v = self
            .db
            .create_saved_view(&req.name, &req.filter.to_string())
            .map_err(db_io)?;
        to_json(&v)
    }

    /// Rename, re-filter and/or hide a view; returns the stored SavedView.
    pub fn update_saved_view_json(&self, request_json: &str) -> Result<String, CoreError> {
        let req: UpdateRequest =
            serde_json::from_str(request_json).map_err(|e| bad("update_saved_view", e))?;
        let filter = req.filter.as_ref().map(|f| f.to_string());
        let v = self
            .db
            .update_saved_view(&req.id, req.name.as_deref(), filter.as_deref(), req.hidden)
            .map_err(db_io)?;
        to_json(&v)
    }

    /// Delete a custom view (built-ins are refused).
    pub fn delete_saved_view(&self, id: &str) -> Result<(), CoreError> {
        self.db.delete_saved_view(id).map_err(db_io)
    }

    /// Reorder views from a JSON array of ids; returns every view (hidden
    /// included) in the new order.
    pub fn reorder_saved_views_json(&self, ids_json: &str) -> Result<String, CoreError> {
        let ids: Vec<String> =
            serde_json::from_str(ids_json).map_err(|e| bad("reorder_saved_views", e))?;
        self.db.reorder_saved_views(&ids).map_err(db_io)?;
        self.list_saved_views_json(true)
    }

    /// Item count of every visible view, in display order, in one query:
    /// `[{id, count}]`.
    pub fn saved_view_counts_json(&self) -> Result<String, CoreError> {
        let now = now_epoch();
        self.saved_view_counts_json_at(now, castellum_db::local_utc_offset_secs(now))
    }

    /// [`Profile::saved_view_counts_json`] with the clock and UTC offset
    /// (seconds) injected.
    pub fn saved_view_counts_json_at(&self, now: i64, tz: i64) -> Result<String, CoreError> {
        to_json(&self.db.saved_view_counts_tz(now, tz).map_err(db_io)?)
    }

    /// The work items of one view (WorkItem array, same order as the
    /// filtered list).
    pub fn saved_view_items_json(&self, id: &str) -> Result<String, CoreError> {
        let now = now_epoch();
        self.saved_view_items_json_at(id, now, castellum_db::local_utc_offset_secs(now))
    }

    /// [`Profile::saved_view_items_json`] with the clock and offset injected.
    pub fn saved_view_items_json_at(
        &self,
        id: &str,
        now: i64,
        tz: i64,
    ) -> Result<String, CoreError> {
        to_json(&self.db.saved_view_items_tz(id, now, tz).map_err(db_io)?)
    }

    /// Unassigned mail: one page of inbox threads with no project or
    /// work-item link, newest first. Ids and counts only.
    /// `{threads:[{thread_id, message_count, last_message_at}], total, limit,
    /// offset}`.
    pub fn unassigned_mail_json(&self, request_json: &str) -> Result<String, CoreError> {
        let req: PageRequest = if request_json.trim().is_empty() {
            PageRequest {
                limit: default_limit(),
                offset: 0,
            }
        } else {
            serde_json::from_str(request_json).map_err(|e| bad("unassigned_mail", e))?
        };
        to_json(
            &self
                .db
                .list_unassigned_mail_threads(req.limit, req.offset)
                .map_err(db_io)?,
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::{open_profile, Profile};
    use serde_json::Value;

    const WED: i64 = 1_799_798_400;

    fn profile(tag: &str) -> Profile {
        let dir = castellum_testutil::scratch_root().join(format!(
            "castellum-saved-views-core-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        open_profile(&dir).unwrap()
    }

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    fn len(s: &str) -> usize {
        parse(s).as_array().unwrap().len()
    }

    #[test]
    fn json_round_trip_create_update_reorder_delete() {
        let p = profile("crud");
        let list = parse(&p.list_saved_views_json(false).unwrap());
        assert_eq!(list.as_array().unwrap().len(), 4);
        assert_eq!(list[0]["name"], "Today");
        assert_eq!(list[0]["built_in"], true);
        assert_eq!(
            list[0]["filter"]["due"],
            serde_json::json!(["overdue", "today"])
        );

        let v = parse(
            &p.create_saved_view_json(r#"{"name":"Blocked","filter":{"states":["blocked"]}}"#)
                .unwrap(),
        );
        let id = v["id"].as_str().unwrap().to_string();
        assert_eq!(v["filter"], serde_json::json!({"states": ["blocked"]}));

        let r = parse(
            &p.update_saved_view_json(&format!(r#"{{"id":"{id}","name":"Stuck","hidden":true}}"#))
                .unwrap(),
        );
        assert_eq!(r["name"], "Stuck");
        assert_eq!(r["hidden"], true);

        let order = format!(r#"["{id}","builtin:someday"]"#);
        let all = parse(&p.reorder_saved_views_json(&order).unwrap());
        assert_eq!(all[0]["name"], "Stuck");
        assert_eq!(all[1]["id"], "builtin:someday");

        p.delete_saved_view(&id).unwrap();
        assert_eq!(len(&p.list_saved_views_json(true).unwrap()), 4);
        assert!(p.delete_saved_view("builtin:today").is_err());
    }

    #[test]
    fn invalid_requests_are_rejected() {
        let p = profile("invalid");
        for bad in [
            r#"{"name":"X","filter":{"nope":1}}"#,
            r#"{"name":"X","filter":{"states":["bogus"]}}"#,
            r#"{"name":"X","extra":1}"#,
            r#"{"name":"","filter":{}}"#,
            "nope",
        ] {
            assert!(p.create_saved_view_json(bad).is_err(), "{bad}");
        }
        assert_eq!(len(&p.list_saved_views_json(true).unwrap()), 4);
        assert!(p
            .update_saved_view_json(r#"{"id":"builtin:today","name":"Now"}"#)
            .is_err());
        assert!(p.reorder_saved_views_json(r#"{"not":"an array"}"#).is_err());
        assert!(p.unassigned_mail_json(r#"{"page":1}"#).is_err());
    }

    #[test]
    fn counts_and_items_use_the_injected_clock() {
        let p = profile("counts");
        for (t, s, due) in [
            ("late", "needs_reply", WED - 3600),
            ("tonight", "waiting", WED + 20 * 3600),
        ] {
            p.upsert_work_item_json_at(
                &format!(r#"{{"title":"{t}","state":"{s}","due_at":{due}}}"#),
                WED,
            )
            .unwrap();
        }
        let noon = WED + 12 * 3600;
        let counts = parse(&p.saved_view_counts_json_at(noon, 0).unwrap());
        assert_eq!(
            counts[0],
            serde_json::json!({"id": "builtin:today", "count": 2})
        );
        assert_eq!(counts[1]["count"], 1); // Needs reply
        assert_eq!(counts[2]["count"], 1); // Waiting on others
        let items = p
            .saved_view_items_json_at("builtin:today", noon, 0)
            .unwrap();
        assert_eq!(len(&items), 2);
        assert!(p.saved_view_items_json_at("nope", WED, 0).is_err());
    }

    #[test]
    fn unassigned_mail_defaults_to_an_empty_first_page() {
        let p = profile("unassigned");
        for req in ["", "{}"] {
            let page = parse(&p.unassigned_mail_json(req).unwrap());
            assert_eq!(page["total"], 0);
            assert_eq!(page["limit"], 50);
            assert_eq!(page["offset"], 0);
        }
    }
}
