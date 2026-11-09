// Castellum - smart mailbox JSON wrappers (S41).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Thin serialization layer over castellum_db::smart_mailboxes, mirroring
// projects.rs. A smart mailbox is a saved S39 query; running one is a
// pass-through to castellum_db::Database::messages_for_smart_mailbox — no
// filter logic lives in this crate either.

use crate::{CoreError, Profile};
use serde::Deserialize;

fn to_json<T: serde::Serialize>(value: &T) -> Result<String, CoreError> {
    serde_json::to_string(value).map_err(|_| CoreError::Io)
}

fn db_io(e: castellum_db::DbError) -> CoreError {
    CoreError::Db(e.to_string())
}

/// Upsert request sent from the UI.
#[derive(Deserialize)]
struct UpsertSmartMailboxRequest {
    #[serde(default)]
    id: Option<String>,
    name: String,
    #[serde(default)]
    query: String,
    #[serde(default)]
    symbol: Option<String>,
    /// MM-1: nested conditions; when present the mailbox is a group mailbox
    /// and `query` may be empty.
    #[serde(default)]
    group: Option<castellum_models::FilterGroup>,
    #[serde(default)]
    scope: Option<castellum_models::SmartMailboxScope>,
}

/// HP-2 Split Inbox request: which Inbox(es) and, for a list, which tab.
#[derive(Deserialize)]
struct InboxSplitRequest {
    /// Inbox mailbox ids shown; empty = every account's Inbox.
    #[serde(default)]
    mailbox_ids: Vec<String>,
    /// A split's id or `"other"`; required for a list, ignored for counts.
    #[serde(default)]
    split_id: Option<String>,
    #[serde(default = "default_split_limit")]
    limit: i64,
}

/// Same cap as a smart mailbox list: split tabs do not page.
fn default_split_limit() -> i64 {
    500
}

fn bad_split_request(e: serde_json::Error) -> CoreError {
    CoreError::Db(format!("bad inbox split json: {e}"))
}

impl Profile {
    /// HP-2: every split's exact total/unread over the requested Inbox(es),
    /// in tab order, then Other (`id == "other"`), as a JSON array of
    /// `InboxSplitCount`. Request: `{"mailbox_ids": [...]}` (empty = all).
    pub fn inbox_split_counts_json(&self, json: &str) -> Result<String, CoreError> {
        let req: InboxSplitRequest = serde_json::from_str(json).map_err(bad_split_request)?;
        to_json(
            &self
                .db
                .inbox_split_counts(&req.mailbox_ids)
                .map_err(db_io)?,
        )
    }

    /// HP-2: one tab's messages (`{"mailbox_ids": [...], "split_id": "...",
    /// "limit": n}`), newest first, as a JSON array of `MessageHeader`; JSON
    /// `null` when the split no longer exists — never an error.
    pub fn inbox_split_messages_json(&self, json: &str) -> Result<String, CoreError> {
        let req: InboxSplitRequest = serde_json::from_str(json).map_err(bad_split_request)?;
        let split_id = req
            .split_id
            .ok_or_else(|| CoreError::Db("bad inbox split json: split_id is required".into()))?;
        to_json(
            &self
                .db
                .inbox_split_messages(&req.mailbox_ids, &split_id, req.limit)
                .map_err(db_io)?,
        )
    }

    /// List smart mailboxes (sidebar order) as a JSON array.
    pub fn list_smart_mailboxes_json(&self) -> Result<String, CoreError> {
        to_json(&self.db.list_smart_mailboxes().map_err(db_io)?)
    }

    /// Recompute every smart mailbox's count (3 queries per mailbox) and
    /// return the updated rows as a JSON array. Added for `read_perf_gate`
    /// (S64)'s `smart_mailbox_recount` budget -- the plan named this
    /// `recompute_all_smart_mailbox_counts_json` but it did not exist yet;
    /// see the plan's Deviations list.
    pub fn recompute_all_smart_mailbox_counts_json(&self) -> Result<String, CoreError> {
        to_json(
            &self
                .db
                .recompute_all_smart_mailbox_counts()
                .map_err(db_io)?,
        )
    }

    /// Create (`id` absent/empty) or rename/re-query (`id` present) a smart
    /// mailbox from `{id?, name, query, symbol?}` JSON; recomputes its
    /// counts immediately (a Save/rename is a deliberate, infrequent action,
    /// never a per-keystroke one) and returns the stored row as JSON.
    pub fn upsert_smart_mailbox_json(&self, json: &str) -> Result<String, CoreError> {
        let req: UpsertSmartMailboxRequest = serde_json::from_str(json)
            .map_err(|e| CoreError::Db(format!("bad upsert_smart_mailbox json: {e}")))?;
        let stored = self
            .db
            .upsert_smart_mailbox(castellum_db::SmartMailboxUpsert {
                id: req.id.as_deref(),
                name: &req.name,
                query: &req.query,
                symbol: req.symbol.as_deref(),
                group: req.group.as_ref(),
                scope: req.scope.as_ref(),
            })
            .map_err(db_io)?;
        let recomputed = self
            .db
            .recompute_smart_mailbox_counts(&stored.id)
            .map_err(db_io)?;
        to_json(&recomputed)
    }

    /// Permanently delete a smart mailbox (never touches any mail).
    pub fn delete_smart_mailbox(&self, id: &str) -> Result<(), CoreError> {
        self.db.delete_smart_mailbox(id).map_err(db_io)
    }

    /// Reorder from a JSON array of ids, e.g. `["id1","id2","id3"]`.
    pub fn reorder_smart_mailboxes_json(&self, json: &str) -> Result<(), CoreError> {
        let ids: Vec<String> = serde_json::from_str(json)
            .map_err(|e| CoreError::Db(format!("bad reorder_smart_mailboxes json: {e}")))?;
        self.db.reorder_smart_mailboxes(&ids).map_err(db_io)
    }

    /// MM-1c header-condition policy for the editor: the single source of truth
    /// for which header names are storable, so Swift keeps no copy to drift.
    /// `known_names` = up to 200 lower-cased header names seen in this profile
    /// (completion only; mail content, never sent anywhere else). Local-only.
    pub fn filter_header_policy_json(&self) -> Result<String, CoreError> {
        use castellum_models as m;
        let builtin: serde_json::Map<String, serde_json::Value> = m::HEADER_BUILTIN_FIELDS
            .iter()
            .map(|(name, f)| {
                let field = serde_json::to_value(f.field()).unwrap_or_default();
                ((*name).to_string(), field)
            })
            .collect();
        let known = self.db.list_known_header_names(200).map_err(db_io)?;
        to_json(&serde_json::json!({
            "max_name_len": m::HEADER_NAME_MAX_LEN,
            "builtin_fields": builtin,
            "not_stored": m::HEADER_NOT_STORED,
            "not_stored_prefixes": m::HEADER_NOT_STORED_PREFIXES,
            "not_stored_substrings": m::HEADER_NOT_STORED_SUBSTRINGS,
            "known_names": known,
        }))
    }

    /// `{"captured":N,"total":M}`: messages whose headers are stored vs all.
    pub fn message_header_coverage_json(&self) -> Result<String, CoreError> {
        to_json(&self.db.message_header_coverage().map_err(db_io)?)
    }

    /// Recompute every smart mailbox's cached counts; returns the refreshed
    /// list as JSON. Called after each sync pass — never per keystroke.
    pub fn refresh_smart_mailbox_counts_json(&self) -> Result<String, CoreError> {
        to_json(
            &self
                .db
                .recompute_all_smart_mailbox_counts()
                .map_err(db_io)?,
        )
    }

    /// Runs one smart mailbox's saved query cross-account (same S39 path as
    /// "All Inboxes" search). JSON `null` when `id` is unknown — never an
    /// error.
    pub fn messages_for_smart_mailbox_json(
        &self,
        id: &str,
        limit: i64,
    ) -> Result<String, CoreError> {
        to_json(
            &self
                .db
                .messages_for_smart_mailbox(id, limit)
                .map_err(db_io)?,
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::open_profile;
    use castellum_models::{Account, Address, Mailbox, NewMessage};

    fn profile(tag: &str) -> crate::Profile {
        let dir = castellum_testutil::scratch_root().join(format!(
            "castellum-smart-mailboxes-core-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        open_profile(&dir).unwrap()
    }

    #[test]
    fn smart_mailbox_json_roundtrip() {
        let p = profile("json_rt");
        p.db.upsert_account(&Account {
            id: "a".into(),
            email: "me@x.test".into(),
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
        p.db.upsert_message(&NewMessage {
            id: "a:INBOX:1".into(),
            account_id: "a".into(),
            mailbox_id: "a:INBOX".into(),
            provider_uid: Some(1),
            rfc822_message_id: Some("<1@x.test>".into()),
            subject: Some("Hi".into()),
            from_name: Some("Alice".into()),
            from_address: Some("alice@x.test".into()),
            sent_at: Some(1_800_000_000),
            snippet: None,
            size_bytes: None,
            has_attachments: false,
            attachments: None,
            is_read: false,
            is_starred: false,
            addresses: vec![Address {
                kind: "from".into(),
                name: Some("Alice".into()),
                address: "alice@x.test".into(),
            }],
            text_body: None,
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: vec![],
        })
        .unwrap();

        // Create — counts recomputed inline.
        let created = p
            .upsert_smart_mailbox_json(r#"{"name":"Alice","query":"from:alice"}"#)
            .unwrap();
        assert!(created.contains("\"name\":\"Alice\""), "{created}");
        assert!(created.contains("\"cached_total\":1"), "{created}");
        let v: serde_json::Value = serde_json::from_str(&created).unwrap();
        let id = v["id"].as_str().unwrap().to_string();

        // List.
        assert!(p.list_smart_mailboxes_json().unwrap().contains(&id));

        // Run — the saved query, cross-account.
        let msgs = p.messages_for_smart_mailbox_json(&id, 50).unwrap();
        assert!(msgs.contains("\"id\":\"a:INBOX:1\""), "{msgs}");

        // Unknown id -> JSON null, not an error.
        assert_eq!(
            p.messages_for_smart_mailbox_json("nope", 50).unwrap(),
            "null"
        );

        // Rename + re-query.
        let renamed = p
            .upsert_smart_mailbox_json(&format!(
                r#"{{"id":"{id}","name":"Alice 2","query":"from:nobody"}}"#
            ))
            .unwrap();
        assert!(renamed.contains("\"name\":\"Alice 2\""), "{renamed}");
        assert!(
            renamed.contains("\"cached_total\":0"),
            "re-query recomputes: {renamed}"
        );

        // Reorder (single-item list — exercises the JSON parse path).
        p.reorder_smart_mailboxes_json(&format!(r#"["{id}"]"#))
            .unwrap();

        // Refresh all.
        let refreshed = p.refresh_smart_mailbox_counts_json().unwrap();
        assert!(refreshed.contains(&id));

        // Delete.
        p.delete_smart_mailbox(&id).unwrap();
        assert_eq!(p.list_smart_mailboxes_json().unwrap(), "[]");
        assert!(p.delete_smart_mailbox("nope").is_err());
    }

    #[test]
    fn inbox_split_json_counts_and_lists() {
        let p = profile("splits_json");
        p.db.upsert_account(&Account {
            id: "a".into(),
            email: "me@x.test".into(),
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
        for (uid, subject) in [(1, "Invoice 1"), (2, "Lunch")] {
            p.db.upsert_message(&NewMessage {
                id: format!("a:INBOX:{uid}"),
                account_id: "a".into(),
                mailbox_id: "a:INBOX".into(),
                provider_uid: Some(uid),
                rfc822_message_id: Some(format!("<{uid}@x.test>")),
                subject: Some(subject.into()),
                from_name: None,
                from_address: Some("alice@x.test".into()),
                sent_at: Some(1_800_000_000 + uid),
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
        let group = r#"{"op":"and","conditions":[{"field":"subject","operator":"contains","value":"invoice"}],"groups":[]}"#;
        let stored = p
            .upsert_smart_mailbox_json(&format!(
                r#"{{"name":"Invoices","group":{group},"scope":{{"inbox_split":true}}}}"#
            ))
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(v["scope"]["inbox_split"], true);
        let id = v["id"].as_str().unwrap().to_string();

        let counts: serde_json::Value =
            serde_json::from_str(&p.inbox_split_counts_json("{}").unwrap()).unwrap();
        assert_eq!(counts[0]["id"], id.as_str());
        assert_eq!(counts[0]["total"], 1);
        assert_eq!(counts[1]["id"], "other");
        assert_eq!(counts[1]["unread"], 1);

        let rows = p
            .inbox_split_messages_json(&format!(r#"{{"split_id":"{id}"}}"#))
            .unwrap();
        assert!(
            rows.contains("a:INBOX:1") && !rows.contains("a:INBOX:2"),
            "{rows}"
        );
        let other = p
            .inbox_split_messages_json(r#"{"mailbox_ids":["a:INBOX"],"split_id":"other"}"#)
            .unwrap();
        assert!(
            other.contains("a:INBOX:2") && !other.contains("a:INBOX:1"),
            "{other}"
        );
        assert_eq!(
            p.inbox_split_messages_json(r#"{"split_id":"gone"}"#)
                .unwrap(),
            "null"
        );
        assert!(
            p.inbox_split_messages_json("{}").is_err(),
            "split_id required"
        );
        assert!(p.inbox_split_counts_json("not json").is_err());
    }

    #[test]
    fn group_mailbox_json_roundtrip_and_rejections() {
        let p = profile("group_json");
        let group = r#"{"op":"none","conditions":[{"field":"from","operator":"contains","value":"spam"}],"groups":[]}"#;
        let stored = p
            .upsert_smart_mailbox_json(&format!(r#"{{"name":"Not spam","group":{group}}}"#))
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(v["query"], "");
        assert_eq!(v["group"]["op"], "none");
        assert_eq!(v["scope"]["mailbox_ids"], serde_json::json!([]));
        // No query and no group, an empty group, and an unknown operator all fail.
        assert!(p.upsert_smart_mailbox_json(r#"{"name":"x"}"#).is_err());
        assert!(p
            .upsert_smart_mailbox_json(
                r#"{"name":"y","group":{"op":"and","conditions":[],"groups":[]}}"#
            )
            .is_err());
        assert!(p
            .upsert_smart_mailbox_json(
                r#"{"name":"z","group":{"op":"xor","conditions":[],"groups":[]}}"#
            )
            .is_err());
    }
}
