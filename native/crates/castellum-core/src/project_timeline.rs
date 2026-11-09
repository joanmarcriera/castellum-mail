// Castellum - project timeline JSON wrappers (Active Work backend).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Thin serialization layer over castellum-db's append-only project_timeline.
// Work-item state changes and message links append entries automatically
// inside the db layer; this layer exposes the explicit writes (an activity
// note, a recorded decision) and the paged read. Entries carry a short
// summary and reference ids only, never message content.
//
// Requests (JSON objects, unknown keys rejected):
//   add       {project_id, kind, summary, ref_kind?, ref_id?}
//             kind = activity | decision (work_item_change is written only
//             by the automatic hooks); ref_kind = message|thread|contact|
//             work_item, required together with ref_id
//   decision  {project_id, summary, ref_kind?, ref_id?}
//   list      {project_id, kinds?, limit?, offset?}   newest first,
//             limit default 50 (max 500), offset default 0

use crate::{CoreError, Profile};
use castellum_db::{EntityKind, TimelineKind};
use serde::Deserialize;

fn to_json<T: serde::Serialize>(value: &T) -> Result<String, CoreError> {
    serde_json::to_string(value).map_err(|_| CoreError::Io)
}

fn db_io(e: castellum_db::DbError) -> CoreError {
    CoreError::Db(e.to_string())
}

fn bad(what: &str, e: serde_json::Error) -> CoreError {
    CoreError::Db(format!("bad {what} json: {e}"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddRequest {
    project_id: String,
    kind: String,
    summary: String,
    #[serde(default)]
    ref_kind: Option<String>,
    #[serde(default)]
    ref_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionRequest {
    project_id: String,
    summary: String,
    #[serde(default)]
    ref_kind: Option<String>,
    #[serde(default)]
    ref_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListRequest {
    project_id: String,
    #[serde(default)]
    kinds: Option<Vec<String>>,
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
}

fn default_limit() -> i64 {
    50
}

/// Validate the optional (ref_kind, ref_id) pair: both or neither.
fn parse_ref(
    kind: Option<&str>,
    id: Option<&str>,
) -> Result<Option<(EntityKind, String)>, CoreError> {
    match (kind, id) {
        (None, None) => Ok(None),
        (Some(k), Some(i)) if !i.is_empty() => {
            let kind = EntityKind::parse(k)
                .ok_or_else(|| CoreError::Db(format!("unknown ref_kind {k:?}")))?;
            Ok(Some((kind, i.to_string())))
        }
        _ => Err(CoreError::Db(
            "ref_kind and a non-empty ref_id must be given together".into(),
        )),
    }
}

impl Profile {
    /// Append an `activity` or `decision` entry; returns the stored
    /// TimelineEntry JSON.
    pub fn add_timeline_entry_json(&self, json: &str) -> Result<String, CoreError> {
        let req: AddRequest =
            serde_json::from_str(json).map_err(|e| bad("add_timeline_entry", e))?;
        let kind = match TimelineKind::parse(&req.kind) {
            Some(k @ (TimelineKind::Activity | TimelineKind::Decision)) => k,
            _ => {
                return Err(CoreError::Db(format!(
                    "kind must be activity or decision, not {:?}",
                    req.kind
                )))
            }
        };
        let r = parse_ref(req.ref_kind.as_deref(), req.ref_id.as_deref())?;
        let entry = self
            .db
            .add_timeline_entry(
                &req.project_id,
                kind,
                &req.summary,
                r.as_ref().map(|(k, i)| (*k, i.as_str())),
            )
            .map_err(db_io)?;
        to_json(&entry)
    }

    /// Record a decision; returns the stored TimelineEntry JSON.
    pub fn record_decision_json(&self, json: &str) -> Result<String, CoreError> {
        let req: DecisionRequest =
            serde_json::from_str(json).map_err(|e| bad("record_decision", e))?;
        let r = parse_ref(req.ref_kind.as_deref(), req.ref_id.as_deref())?;
        let entry = self
            .db
            .record_decision(
                &req.project_id,
                &req.summary,
                r.as_ref().map(|(k, i)| (*k, i.as_str())),
            )
            .map_err(db_io)?;
        to_json(&entry)
    }

    /// One page of a project's timeline, newest first, as a JSON array of
    /// TimelineEntry `{id, project_id, kind, summary, ref_type, ref_id,
    /// created_at}`.
    pub fn list_timeline_json(&self, json: &str) -> Result<String, CoreError> {
        let req: ListRequest = serde_json::from_str(json).map_err(|e| bad("list_timeline", e))?;
        let kinds = match &req.kinds {
            None => None,
            Some(ks) => Some(
                ks.iter()
                    .map(|k| {
                        TimelineKind::parse(k)
                            .ok_or_else(|| CoreError::Db(format!("unknown kind {k:?}")))
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
        };
        let rows = self
            .db
            .list_timeline(&req.project_id, kinds.as_deref(), req.limit, req.offset)
            .map_err(db_io)?;
        to_json(&rows)
    }
}

#[cfg(test)]
mod tests {
    use crate::{open_profile, Profile};
    use serde_json::Value;

    fn profile(tag: &str) -> Profile {
        let dir = castellum_testutil::scratch_root().join(format!(
            "castellum-timeline-core-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        open_profile(&dir).unwrap()
    }

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    fn project(p: &Profile) -> String {
        let v = parse(
            &p.upsert_project_json(r#"{"name":"P","status":"active","position":0}"#)
                .unwrap(),
        );
        v["id"].as_str().unwrap().to_string()
    }

    #[test]
    fn add_decision_and_list_round_trip() {
        let p = profile("rt");
        let pid = project(&p);
        let a = parse(
            &p.add_timeline_entry_json(&format!(
                r#"{{"project_id":"{pid}","kind":"activity","summary":"Kickoff call"}}"#
            ))
            .unwrap(),
        );
        assert_eq!(a["kind"], "activity");
        assert!(a["ref_type"].is_null());
        let d = parse(
            &p.record_decision_json(&format!(
                r#"{{"project_id":"{pid}","summary":"Ship in Q4","ref_kind":"contact","ref_id":"7"}}"#
            ))
            .unwrap(),
        );
        assert_eq!(d["kind"], "decision");
        assert_eq!(d["ref_type"], "contact");
        assert_eq!(d["ref_id"], "7");

        let all = parse(
            &p.list_timeline_json(&format!(r#"{{"project_id":"{pid}"}}"#))
                .unwrap(),
        );
        assert_eq!(all.as_array().unwrap().len(), 2);
        let only = parse(
            &p.list_timeline_json(&format!(
                r#"{{"project_id":"{pid}","kinds":["decision"],"limit":1,"offset":0}}"#
            ))
            .unwrap(),
        );
        assert_eq!(only.as_array().unwrap().len(), 1);
        assert_eq!(only[0]["summary"], "Ship in Q4");
        let page2 = parse(
            &p.list_timeline_json(&format!(r#"{{"project_id":"{pid}","limit":1,"offset":1}}"#))
                .unwrap(),
        );
        assert_eq!(page2.as_array().unwrap().len(), 1);
    }

    #[test]
    fn work_item_changes_appear_via_json_hooks() {
        let p = profile("hooks");
        let pid = project(&p);
        let w = parse(
            &p.upsert_work_item_json(&format!(
                r#"{{"project_id":"{pid}","title":"Reply to Bob","state":"needs_reply"}}"#
            ))
            .unwrap(),
        );
        let wid = w["id"].as_str().unwrap();
        p.upsert_work_item_json(&format!(
            r#"{{"id":"{wid}","project_id":"{pid}","title":"Reply to Bob","state":"done"}}"#
        ))
        .unwrap();
        let rows = parse(
            &p.list_timeline_json(&format!(
                r#"{{"project_id":"{pid}","kinds":["work_item_change"]}}"#
            ))
            .unwrap(),
        );
        assert_eq!(rows.as_array().unwrap().len(), 2);
        assert_eq!(rows[0]["ref_id"], wid);
        assert!(rows[0]["summary"].as_str().unwrap().contains("done"));
    }

    #[test]
    fn invalid_requests_are_rejected() {
        let p = profile("bad");
        let pid = project(&p);
        for bad in [
            format!(r#"{{"project_id":"{pid}","kind":"work_item_change","summary":"x"}}"#),
            format!(r#"{{"project_id":"{pid}","kind":"bogus","summary":"x"}}"#),
            format!(r#"{{"project_id":"{pid}","kind":"activity","summary":"  "}}"#),
            format!(
                r#"{{"project_id":"{pid}","kind":"activity","summary":"x","ref_kind":"message"}}"#
            ),
            format!(
                r#"{{"project_id":"{pid}","kind":"activity","summary":"x","ref_kind":"project","ref_id":"a"}}"#
            ),
            format!(
                r#"{{"project_id":"{pid}","kind":"activity","summary":"x","ref_kind":"nope","ref_id":"a"}}"#
            ),
            r#"{"project_id":"missing","kind":"activity","summary":"x"}"#.to_string(),
            format!(r#"{{"project_id":"{pid}","kind":"activity","summary":"x","extra":1}}"#),
            "nope".to_string(),
        ] {
            assert!(p.add_timeline_entry_json(&bad).is_err(), "{bad}");
        }
        assert!(p
            .record_decision_json(r#"{"project_id":"x","summary":"s","extra":1}"#)
            .is_err());
        assert!(p
            .list_timeline_json(&format!(r#"{{"project_id":"{pid}","kinds":["bogus"]}}"#))
            .is_err());
        assert!(p
            .list_timeline_json(&format!(r#"{{"project_id":"{pid}","page":1}}"#))
            .is_err());
        // Nothing was written by any rejected call.
        let rows = parse(
            &p.list_timeline_json(&format!(r#"{{"project_id":"{pid}"}}"#))
                .unwrap(),
        );
        assert!(rows.as_array().unwrap().is_empty());
    }
}
