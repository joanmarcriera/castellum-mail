// Castellum - shared link model JSON wrappers.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Thin serialization layer over the generic `links` repository (migration
// 0017), mirroring projects.rs. Entity kinds are validated here so the FFI
// can never write an unknown endpoint kind.

use crate::{CoreError, Profile};
use castellum_db::{EntityKind, LinkEnd, LinkUpsert};
use serde::Deserialize;

fn to_json<T: serde::Serialize>(value: &T) -> Result<String, CoreError> {
    serde_json::to_string(value).map_err(|_| CoreError::Io)
}

fn db_io(e: castellum_db::DbError) -> CoreError {
    CoreError::Db(e.to_string())
}

fn parse_kind(s: &str) -> Result<EntityKind, CoreError> {
    EntityKind::parse(s).ok_or_else(|| CoreError::Db(format!("unknown entity kind {s:?}")))
}

/// Endpoints + relation naming one link (unlink / note requests).
#[derive(Deserialize)]
struct LinkRefRequest {
    src_kind: String,
    src_id: String,
    dst_kind: String,
    dst_id: String,
    relation: String,
}

/// Upsert request. `origin` defaults to 'manual'; provenance/confidence are
/// optional and only meaningful for non-manual origins.
#[derive(Deserialize)]
struct UpsertLinkRequest {
    src_kind: String,
    src_id: String,
    dst_kind: String,
    dst_id: String,
    relation: String,
    #[serde(default = "default_origin")]
    origin: String,
    #[serde(default)]
    provenance: Option<String>,
    #[serde(default)]
    confidence: Option<f64>,
}

/// Note request: `note` null/absent clears the note.
#[derive(Deserialize)]
struct LinkNoteRequest {
    src_kind: String,
    src_id: String,
    dst_kind: String,
    dst_id: String,
    relation: String,
    #[serde(default)]
    note: Option<String>,
}

fn default_origin() -> String {
    "manual".into()
}

impl Profile {
    /// Idempotently create/refresh a link from JSON; returns the stored row.
    /// Shape: `{src_kind, src_id, dst_kind, dst_id, relation, origin?,
    /// provenance?, confidence?}` (origin defaults to 'manual').
    pub fn upsert_link_json(&self, json: &str) -> Result<String, CoreError> {
        let req: UpsertLinkRequest = serde_json::from_str(json)
            .map_err(|e| CoreError::Db(format!("bad upsert_link json: {e}")))?;
        let record = self
            .db
            .upsert_link(LinkUpsert {
                src: LinkEnd::new(parse_kind(&req.src_kind)?, &req.src_id),
                dst: LinkEnd::new(parse_kind(&req.dst_kind)?, &req.dst_id),
                relation: &req.relation,
                origin: &req.origin,
                provenance: req.provenance.as_deref(),
                confidence: req.confidence,
            })
            .map_err(db_io)?;
        to_json(&record)
    }

    /// Remove a link named by `{src_kind, src_id, dst_kind, dst_id, relation}`.
    /// Idempotent; returns whether a row existed.
    pub fn unlink_json(&self, json: &str) -> Result<bool, CoreError> {
        let req: LinkRefRequest = serde_json::from_str(json)
            .map_err(|e| CoreError::Db(format!("bad unlink json: {e}")))?;
        self.db
            .unlink(
                LinkEnd::new(parse_kind(&req.src_kind)?, &req.src_id),
                LinkEnd::new(parse_kind(&req.dst_kind)?, &req.dst_id),
                &req.relation,
            )
            .map_err(db_io)
    }

    /// Set or clear the user-owned note on an existing link.
    /// Shape: `{src_kind, src_id, dst_kind, dst_id, relation, note?}`.
    pub fn set_link_note_json(&self, json: &str) -> Result<(), CoreError> {
        let req: LinkNoteRequest = serde_json::from_str(json)
            .map_err(|e| CoreError::Db(format!("bad link note json: {e}")))?;
        self.db
            .set_link_note(
                LinkEnd::new(parse_kind(&req.src_kind)?, &req.src_id),
                LinkEnd::new(parse_kind(&req.dst_kind)?, &req.dst_id),
                &req.relation,
                req.note.as_deref(),
            )
            .map_err(db_io)
    }

    /// Every link touching `(kind, id)` in either direction as a JSON array
    /// of LinkRecord (origin, provenance, confidence, note, timestamps) —
    /// the "why is this connected?" read path.
    pub fn links_for_json(&self, kind: &str, id: &str) -> Result<String, CoreError> {
        let records = self
            .db
            .links_for(LinkEnd::new(parse_kind(kind)?, id))
            .map_err(db_io)?;
        to_json(&records)
    }
}

#[cfg(test)]
mod tests {
    use crate::open_profile;
    use castellum_models::{Account, Mailbox, NewMessage};

    fn profile(tag: &str) -> crate::Profile {
        let dir = castellum_testutil::scratch_root().join(format!(
            "castellum-links-core-{}-{}",
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

    /// Endpoint existence is enforced on write, so tests link only real rows.
    /// Seed one account/mailbox/message; return the message id.
    fn seed_message(p: &crate::Profile, id: &str) -> String {
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
        id.to_string()
    }

    fn seed_project(p: &crate::Profile) -> String {
        let v = parse(
            &p.upsert_project_json(r#"{"name":"P","status":"active"}"#)
                .unwrap(),
        );
        v["id"].as_str().unwrap().to_string()
    }

    fn seed_contact(p: &crate::Profile, email: &str) -> String {
        let v = parse(
            &p.create_contact_json(&format!(r#"{{"email":"{email}"}}"#))
                .unwrap(),
        );
        v["id"].to_string()
    }

    #[test]
    fn upsert_links_for_and_unlink_round_trip() {
        let p = profile("roundtrip");
        let mid = seed_message(&p, "acc:INBOX:1");
        let pid = seed_project(&p);
        let stored = p
            .upsert_link_json(&format!(
                r#"{{"src_kind":"project","src_id":"{pid}",
                    "dst_kind":"message","dst_id":"{mid}",
                    "relation":"includes"}}"#
            ))
            .unwrap();
        let v = parse(&stored);
        assert_eq!(v["origin"], "manual", "origin defaults to manual");
        assert_eq!(v["src_kind"], "project");
        assert_eq!(v["note"], serde_json::Value::Null);

        let listed = p.links_for_json("project", &pid).unwrap();
        let arr = parse(&listed);
        assert_eq!(arr.as_array().unwrap().len(), 1);
        assert_eq!(arr[0]["dst_id"], mid);

        let removed = p
            .unlink_json(&format!(
                r#"{{"src_kind":"project","src_id":"{pid}",
                    "dst_kind":"message","dst_id":"{mid}",
                    "relation":"includes"}}"#
            ))
            .unwrap();
        assert!(removed);
        assert_eq!(
            parse(&p.links_for_json("project", &pid).unwrap()),
            parse("[]")
        );
    }

    #[test]
    fn derived_upsert_carries_evidence_and_note_survives() {
        let p = profile("evidence");
        seed_message(&p, "acc:INBOX:9");
        let pid = seed_project(&p);
        let cid = seed_contact(&p, "c@x.test");
        p.upsert_link_json(&format!(
            r#"{{"src_kind":"project","src_id":"{pid}",
                "dst_kind":"contact","dst_id":"{cid}",
                "relation":"involves","origin":"derived",
                "provenance":"message:acc:INBOX:9","confidence":0.8}}"#
        ))
        .unwrap();
        p.set_link_note_json(&format!(
            r#"{{"src_kind":"project","src_id":"{pid}",
                "dst_kind":"contact","dst_id":"{cid}",
                "relation":"involves","note":"met at FOSDEM"}}"#
        ))
        .unwrap();
        // A derived refresh keeps the note.
        let refreshed = p
            .upsert_link_json(&format!(
                r#"{{"src_kind":"project","src_id":"{pid}",
                    "dst_kind":"contact","dst_id":"{cid}",
                    "relation":"involves","origin":"derived",
                    "provenance":"message:acc:INBOX:9","confidence":0.9}}"#
            ))
            .unwrap();
        let v = parse(&refreshed);
        assert_eq!(v["provenance"], "message:acc:INBOX:9");
        assert_eq!(v["confidence"], 0.9);
        assert_eq!(v["note"], "met at FOSDEM");
    }

    #[test]
    fn work_item_links_round_trip_over_json() {
        let p = profile("workitem");
        let pid = seed_project(&p);
        let wi = parse(
            &p.upsert_work_item_json(&format!(
                r#"{{"project_id":"{pid}","title":"T","state":"waiting"}}"#
            ))
            .unwrap(),
        );
        let wid = wi["id"].as_str().unwrap().to_string();
        p.upsert_link_json(&format!(
            r#"{{"src_kind":"work_item","src_id":"{wid}",
                "dst_kind":"project","dst_id":"{pid}","relation":"belongs_to"}}"#
        ))
        .unwrap();
        let arr = parse(&p.links_for_json("work_item", &wid).unwrap());
        assert_eq!(arr.as_array().unwrap().len(), 1);
        assert_eq!(arr[0]["dst_kind"], "project");
        assert_eq!(arr[0]["src_kind"], "work_item");
    }

    #[test]
    fn unknown_kind_bad_json_and_dangling_endpoints_are_rejected() {
        let p = profile("badinput");
        // Unknown entity kind.
        assert!(p
            .upsert_link_json(
                r#"{"src_kind":"folder","src_id":"x",
                    "dst_kind":"message","dst_id":"m","relation":"includes"}"#,
            )
            .is_err());
        assert!(p.upsert_link_json("not json").is_err());
        assert!(p.links_for_json("banana", "x").is_err());
        // Valid kinds but nonexistent endpoints are rejected on write.
        let pid = seed_project(&p);
        assert!(
            p.upsert_link_json(&format!(
                r#"{{"src_kind":"project","src_id":"{pid}",
                    "dst_kind":"message","dst_id":"acc:INBOX:404","relation":"includes"}}"#
            ))
            .is_err(),
            "linking a project to a nonexistent message must fail"
        );
    }
}
