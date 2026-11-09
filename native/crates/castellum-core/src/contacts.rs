// Castellum - contacts APIs (JSON over the FFI).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Thin serialization layer over the db contacts repository, mirroring query.rs.
// Refresh recomputes derived data only; user-owned fields cross via
// update_contact_json and are never touched by a refresh.

use crate::enrich::EnrichmentFieldOut;
use crate::{CoreError, Profile};
use castellum_models::ContactRecord;
use serde::{Deserialize, Serialize};

/// Full contact detail response: all `ContactRecord` fields flattened at the
/// top level, plus an `enrichment` array. The `#[serde(flatten)]` mirrors the
/// Swift DTO that decodes both in one pass (no nested "contact" key).
#[derive(Serialize)]
struct ContactDetailResponse {
    #[serde(flatten)]
    contact: ContactRecord,
    enrichment: Vec<EnrichmentFieldOut>,
}

fn to_json<T: Serialize>(value: &T) -> Result<String, CoreError> {
    serde_json::to_string(value).map_err(|_| CoreError::Io)
}

fn db_io(e: castellum_db::DbError) -> CoreError {
    CoreError::Db(e.to_string())
}

/// Unix epoch seconds now (0 on a pre-1970 clock, which only skews decay).
fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// User-field update request (`castellum_update_contact` payload).
#[derive(Deserialize)]
struct UpdateContactRequest {
    id: i64,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    is_hidden: bool,
}

/// Manual contact creation request (`castellum_create_contact` payload).
#[derive(Deserialize)]
struct CreateContactRequest {
    email: String,
    #[serde(default)]
    name: Option<String>,
}

impl Profile {
    /// Recompute the contacts aggregate + edge graph; returns a JSON
    /// AggregateReport {contacts_upserted, edges_rebuilt, elapsed_ms}.
    pub fn refresh_contacts_json(&self) -> Result<String, CoreError> {
        to_json(
            &self
                .db
                .refresh_contact_aggregates(now_epoch())
                .map_err(db_io)?,
        )
    }

    /// Contacts (most recent first) as a JSON array. `query` substring-matches
    /// email/derived_name/display_name.
    pub fn list_contacts_json(
        &self,
        query: Option<&str>,
        include_hidden: bool,
        limit: i64,
    ) -> Result<String, CoreError> {
        let query = query.map(str::trim).filter(|q| !q.is_empty());
        to_json(
            &self
                .db
                .list_contacts(query, include_hidden, limit.max(0))
                .map_err(db_io)?,
        )
    }

    /// Recipients of sent/queued mail (newest first) as a JSON array of
    /// `{email, name, last_sent_at, sent_count}` -- address-autocomplete source.
    pub fn list_sent_recipients_json(&self, limit: i64) -> Result<String, CoreError> {
        to_json(&self.db.list_sent_recipients(limit).map_err(db_io)?)
    }

    /// One contact by id as JSON (`null` if not found). The response includes
    /// all contact fields plus an `enrichment` array of enrichment rows for
    /// this contact. Returns the JSON string `"null"` if the contact is absent.
    pub fn get_contact_json(&self, id: i64) -> Result<String, CoreError> {
        let contact = self.db.get_contact_detail(id).map_err(db_io)?;
        match contact {
            None => Ok("null".to_string()),
            Some(c) => {
                let enrichment = self
                    .db
                    .list_enrichment(c.id)
                    .map_err(db_io)?
                    .into_iter()
                    .map(EnrichmentFieldOut::from)
                    .collect::<Vec<_>>();
                to_json(&ContactDetailResponse {
                    contact: c,
                    enrichment,
                })
            }
        }
    }

    /// Update a contact's user-owned fields from `{id, display_name?, notes?,
    /// is_hidden?}` JSON. Never touches derived columns.
    pub fn update_contact_json(&self, json: &str) -> Result<(), CoreError> {
        let req: UpdateContactRequest = serde_json::from_str(json)
            .map_err(|e| CoreError::Db(format!("bad update_contact json: {e}")))?;
        self.db
            .update_contact_user_fields(req.id, req.display_name, req.notes, req.is_hidden)
            .map_err(db_io)
    }

    /// Create a contact by hand from `{email, name?}` JSON; returns the stored
    /// contact as JSON (is_user_created = true).
    pub fn create_contact_json(&self, json: &str) -> Result<String, CoreError> {
        let req: CreateContactRequest = serde_json::from_str(json)
            .map_err(|e| CoreError::Db(format!("bad create_contact json: {e}")))?;
        to_json(
            &self
                .db
                .create_contact(&req.email, req.name.as_deref())
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
            "castellum-contacts-core-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        open_profile(&dir).unwrap()
    }

    #[test]
    fn contacts_json_roundtrip() {
        let p = profile("json");
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
            addresses: vec![
                Address {
                    kind: "from".into(),
                    name: Some("Alice".into()),
                    address: "alice@x.test".into(),
                },
                Address {
                    kind: "to".into(),
                    name: None,
                    address: "me@x.test".into(),
                },
            ],
            text_body: None,
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: Vec::new(),
        })
        .unwrap();

        let report = p.refresh_contacts_json().unwrap();
        assert!(report.contains("\"contacts_upserted\":2"), "{report}");

        let list = p.list_contacts_json(Some("alice"), true, 100).unwrap();
        assert!(list.contains("\"email\":\"alice@x.test\""), "{list}");
        assert!(list.contains("\"derived_name\":\"Alice\""), "{list}");
        // Blank query means "no filter".
        let all = p.list_contacts_json(Some("   "), true, 100).unwrap();
        assert!(all.contains("me@x.test") && all.contains("alice@x.test"));

        let created = p
            .create_contact_json(r#"{"email":"Bob@X.Test","name":"Bob"}"#)
            .unwrap();
        assert!(created.contains("\"email\":\"bob@x.test\""), "{created}");
        assert!(created.contains("\"is_user_created\":true"), "{created}");
        let v: serde_json::Value = serde_json::from_str(&created).unwrap();
        let id = v["id"].as_i64().unwrap();

        p.update_contact_json(
            &serde_json::json!({"id": id, "display_name": "Bobby", "notes": "n", "is_hidden": true})
                .to_string(),
        )
        .unwrap();
        let got = p.get_contact_json(id).unwrap();
        assert!(got.contains("\"display_name\":\"Bobby\""), "{got}");
        assert!(got.contains("\"is_hidden\":true"), "{got}");

        assert_eq!(p.get_contact_json(999_999).unwrap(), "null");
        assert!(p.update_contact_json("not json").is_err());
        assert!(p.create_contact_json("{}").is_err());
    }
}
