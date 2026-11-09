// Castellum - projects JSON wrappers (slice 8b).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Thin serialization layer over the DB project repository, mirroring contacts.rs.

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
struct UpsertProjectRequest {
    #[serde(default)]
    id: Option<String>,
    name: String,
    #[serde(default)]
    notes: String,
    #[serde(default = "default_status")]
    status: String,
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    position: i64,
    /// When present, sets the archived flag; absent leaves it unchanged.
    #[serde(default)]
    archived: Option<bool>,
}

fn default_status() -> String {
    "active".into()
}

impl Profile {
    /// List projects (with email/contact counts) as a JSON array.
    /// Archived projects are excluded unless `include_archived` is true.
    pub fn list_projects_json(&self, include_archived: bool) -> Result<String, CoreError> {
        to_json(
            &self
                .db
                .list_projects_with_counts(include_archived)
                .map_err(db_io)?,
        )
    }

    /// Create or update a project from JSON; returns the stored project as JSON.
    /// JSON shape: `{id?, name, notes?, status?, color?, position?, archived?}`.
    /// `archived` omitted/null leaves the stored flag unchanged.
    pub fn upsert_project_json(&self, json: &str) -> Result<String, CoreError> {
        let req: UpsertProjectRequest = serde_json::from_str(json)
            .map_err(|e| CoreError::Db(format!("bad upsert_project json: {e}")))?;
        to_json(
            &self
                .db
                .upsert_project(castellum_db::ProjectUpsert {
                    id: req.id.as_deref(),
                    name: &req.name,
                    notes: &req.notes,
                    status: &req.status,
                    color: req.color.as_deref(),
                    position: req.position,
                    archived: req.archived,
                })
                .map_err(db_io)?,
        )
    }

    /// Fetch a project with its linked emails and contacts as JSON (`null` if
    /// not found).
    pub fn get_project_json(&self, id: &str) -> Result<String, CoreError> {
        to_json(&self.db.get_project_hydrated(id).map_err(db_io)?)
    }

    /// Permanently delete a project.
    pub fn delete_project(&self, id: &str) -> Result<(), CoreError> {
        self.db.delete_project(id).map_err(db_io)
    }

    /// Link or unlink a message from a project. Optionally auto-inserts the
    /// message's non-self, non-hidden participants as `origin='auto'` contacts.
    pub fn project_set_email_link(
        &self,
        project_id: &str,
        message_id: &str,
        linked: bool,
        auto_contacts: bool,
    ) -> Result<(), CoreError> {
        self.db
            .set_email_link(project_id, message_id, linked, auto_contacts)
            .map_err(db_io)
    }

    /// Link or unlink a contact from a project (`origin='manual'`).
    pub fn project_set_contact_link(
        &self,
        project_id: &str,
        contact_id: i64,
        linked: bool,
    ) -> Result<(), CoreError> {
        self.db
            .set_contact_link(project_id, contact_id, linked)
            .map_err(db_io)
    }

    /// Projects containing `message_id` as a JSON array of `{project_id, name, color}`.
    pub fn projects_for_message_json(&self, message_id: &str) -> Result<String, CoreError> {
        to_json(&self.db.projects_for_message(message_id).map_err(db_io)?)
    }

    /// Move a project to a new Kanban column and position.
    pub fn move_project(&self, id: &str, status: &str, position: i64) -> Result<(), CoreError> {
        self.db.move_project(id, status, position).map_err(db_io)
    }
}

#[cfg(test)]
mod tests {
    use crate::open_profile;
    use castellum_models::{Account, Address, Mailbox, NewMessage};

    fn profile(tag: &str) -> crate::Profile {
        let dir = castellum_testutil::scratch_root().join(format!(
            "castellum-projects-core-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        open_profile(&dir).unwrap()
    }

    #[test]
    fn projects_json_roundtrip() {
        let p = profile("json_rt");

        // Create.
        let created = p
            .upsert_project_json(r#"{"name":"Alpha","status":"active"}"#)
            .unwrap();
        assert!(created.contains("\"name\":\"Alpha\""), "{created}");
        let v: serde_json::Value = serde_json::from_str(&created).unwrap();
        let pid = v["id"].as_str().unwrap().to_string();
        assert!(!pid.is_empty());

        // List.
        let list = p.list_projects_json(false).unwrap();
        assert!(list.contains("\"name\":\"Alpha\""), "{list}");

        // Update.
        let updated = p
            .upsert_project_json(&format!(
                r#"{{"id":"{pid}","name":"Alpha 2","status":"waiting","notes":"n"}}"#
            ))
            .unwrap();
        assert!(updated.contains("\"name\":\"Alpha 2\""), "{updated}");

        // Get hydrated (null bodies initially).
        let detail = p.get_project_json(&pid).unwrap();
        assert!(detail.contains("\"name\":\"Alpha 2\""), "{detail}");
        assert!(detail.contains("\"emails\":[]"), "{detail}");

        // Archive via JSON: flag persists and hides from the default list.
        let archived = p
            .upsert_project_json(&format!(
                r#"{{"id":"{pid}","name":"Alpha 2","archived":true}}"#
            ))
            .unwrap();
        assert!(archived.contains("\"archived\":true"), "{archived}");
        assert_eq!(p.list_projects_json(false).unwrap(), "[]");
        assert!(p.list_projects_json(true).unwrap().contains(&pid));

        // Upsert WITHOUT archived leaves the flag alone.
        let still = p
            .upsert_project_json(&format!(r#"{{"id":"{pid}","name":"Alpha 3"}}"#))
            .unwrap();
        assert!(still.contains("\"archived\":true"), "{still}");

        // Unarchive.
        let restored = p
            .upsert_project_json(&format!(
                r#"{{"id":"{pid}","name":"Alpha 3","archived":false}}"#
            ))
            .unwrap();
        assert!(restored.contains("\"archived\":false"), "{restored}");

        // Delete.
        p.delete_project(&pid).unwrap();
        assert_eq!(p.list_projects_json(false).unwrap(), "[]");

        // Delete non-existent is an error.
        assert!(p.delete_project("not-an-id").is_err());
    }

    #[test]
    fn projects_for_message_and_email_link() {
        let p = profile("msg_link");

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
        // Ensure contacts are seeded so auto_contacts path works.
        p.db.refresh_contact_aggregates(1_800_000_000).unwrap();

        let proj = p
            .upsert_project_json(r#"{"name":"Work","status":"active"}"#)
            .unwrap();
        let pid: String = serde_json::from_str::<serde_json::Value>(&proj).unwrap()["id"]
            .as_str()
            .unwrap()
            .into();

        p.project_set_email_link(&pid, "a:INBOX:1", true, true)
            .unwrap();

        // Reverse lookup.
        let tags = p.projects_for_message_json("a:INBOX:1").unwrap();
        assert!(tags.contains("\"name\":\"Work\""), "{tags}");
        assert!(
            tags.contains(&format!("\"project_id\":\"{pid}\"")),
            "{tags}"
        );

        // Hydrated detail should include the message header.
        let detail = p.get_project_json(&pid).unwrap();
        assert!(detail.contains("\"id\":\"a:INBOX:1\""), "{detail}");
        // alice@x.test should be auto-linked (non-self participant).
        assert!(detail.contains("\"email\":\"alice@x.test\""), "{detail}");
        assert!(detail.contains("\"origin\":\"auto\""), "{detail}");

        // Unlink.
        p.project_set_email_link(&pid, "a:INBOX:1", false, false)
            .unwrap();
        let after = p.get_project_json(&pid).unwrap();
        assert!(after.contains("\"emails\":[]"), "{after}");

        // Move project.
        p.move_project(&pid, "done", 0).unwrap();
        let list = p.list_projects_json(false).unwrap();
        assert!(list.contains("\"status\":\"done\""), "{list}");
    }
}
