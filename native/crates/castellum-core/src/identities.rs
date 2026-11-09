// Castellum - per-account identity APIs (JSON over the FFI, S47).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Thin serialization layer over the db identities repository, mirroring
// contacts.rs.

use crate::{CoreError, Profile};
use castellum_models::NewIdentity;
use serde::Deserialize;

fn to_json<T: serde::Serialize>(value: &T) -> Result<String, CoreError> {
    serde_json::to_string(value).map_err(|_| CoreError::Io)
}

fn db_io(e: castellum_db::DbError) -> CoreError {
    CoreError::Db(e.to_string())
}

/// Update request (`castellum_update_identity` payload): absent = keep, "" = clear.
#[derive(Deserialize)]
struct UpdateIdentityRequest {
    id: i64,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    reply_to: Option<String>,
    #[serde(default)]
    bcc: Option<String>,
    #[serde(default)]
    is_default: Option<bool>,
}

/// Refuses an address the SMTP builder would later reject, so a bad Reply-To/Bcc
/// fails here, once, rather than on every send from the identity.
fn check_sendable(required: &[&str], optional: &[Option<&str>]) -> Result<(), CoreError> {
    let bad = required
        .iter()
        .copied()
        .chain(
            optional
                .iter()
                .flatten()
                .copied()
                .map(str::trim)
                .filter(|v| !v.is_empty()),
        )
        .find(|a| !castellum_provider_smtp::is_sendable_address(a));
    match bad {
        Some(a) => Err(CoreError::Db(format!("not a sendable address: {a:?}"))),
        None => Ok(()),
    }
}

impl Profile {
    /// Identities for `account_id` as a JSON array. The account's own
    /// address is NOT included — see migration 0026's doc comment.
    pub fn list_identities_json(&self, account_id: &str) -> Result<String, CoreError> {
        to_json(&self.db.list_identities(account_id).map_err(db_io)?)
    }

    /// Create an identity from `{account_id, email, display_name?}` JSON;
    /// returns the stored row as JSON.
    pub fn create_identity_json(&self, json: &str) -> Result<String, CoreError> {
        let req: NewIdentity = serde_json::from_str(json)
            .map_err(|e| CoreError::Db(format!("bad identity json: {e}")))?;
        check_sendable(
            &[req.email.trim()],
            &[req.reply_to.as_deref(), req.bcc.as_deref()],
        )?;
        to_json(&self.db.create_identity(&req).map_err(db_io)?)
    }

    /// Update an identity from `{id, display_name?, reply_to?, bcc?, is_default?}`
    /// JSON: an absent field is left alone, an empty string clears display
    /// name / reply-to / bcc (MM-6).
    pub fn update_identity_json(&self, json: &str) -> Result<(), CoreError> {
        let req: UpdateIdentityRequest = serde_json::from_str(json)
            .map_err(|e| CoreError::Db(format!("bad update_identity json: {e}")))?;
        check_sendable(&[], &[req.reply_to.as_deref(), req.bcc.as_deref()])?;
        self.db
            .update_identity_details(
                req.id,
                req.display_name.as_deref(),
                req.reply_to.as_deref(),
                req.bcc.as_deref(),
                req.is_default,
            )
            .map_err(db_io)
    }

    /// Delete an identity by id.
    pub fn delete_identity(&self, id: i64) -> Result<(), CoreError> {
        self.db.delete_identity(id).map_err(db_io)
    }
}

#[cfg(test)]
mod tests {
    use crate::open_profile;
    use castellum_models::Account;

    fn profile(tag: &str) -> crate::Profile {
        let dir = castellum_testutil::scratch_root().join(format!(
            "castellum-identities-core-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        open_profile(&dir).unwrap()
    }

    #[test]
    fn identity_crud_round_trips_over_json() {
        let p = profile("crud");
        p.db.upsert_account(&Account {
            id: "a".into(),
            email: "e@x".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();

        assert_eq!(p.list_identities_json("a").unwrap(), "[]");
        let created = p
            .create_identity_json(
                &serde_json::json!({"account_id": "a", "email": "alias@x", "display_name": "Alias"})
                    .to_string(),
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&created).unwrap();
        let id = parsed["id"].as_i64().unwrap();
        assert_eq!(parsed["email"], "alias@x");

        p.update_identity_json(
            &serde_json::json!({"id": id, "display_name": "New Name"}).to_string(),
        )
        .unwrap();
        let list: serde_json::Value =
            serde_json::from_str(&p.list_identities_json("a").unwrap()).unwrap();
        assert_eq!(list[0]["display_name"], "New Name");

        p.delete_identity(id).unwrap();
        assert_eq!(p.list_identities_json("a").unwrap(), "[]");
    }

    #[test]
    fn unsendable_or_injected_addresses_are_refused_at_save() {
        let p = profile("sendable");
        p.db.upsert_account(&Account {
            id: "a".into(),
            email: "me@x.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        for bad in ["a@b@c", "x@", "no-domain@", "a@b.test\r\nBcc: e@x.test"] {
            let req =
                serde_json::json!({"account_id":"a","email":"alias@x.test","bcc":bad}).to_string();
            assert!(p.create_identity_json(&req).is_err(), "{bad:?}");
        }
        let ok = p.create_identity_json(&serde_json::json!({"account_id":"a","email":"alias@x.test","reply_to":"team@x.test","is_default":true}).to_string()).unwrap();
        let id = serde_json::from_str::<serde_json::Value>(&ok).unwrap()["id"]
            .as_i64()
            .unwrap();
        assert!(p
            .update_identity_json(&serde_json::json!({"id":id,"reply_to":"a@b@c"}).to_string())
            .is_err());
        p.update_identity_json(
            &serde_json::json!({"id":id,"reply_to":"","is_default":false}).to_string(),
        )
        .unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&p.list_identities_json("a").unwrap()).unwrap();
        assert!(v[0]["reply_to"].is_null() && v[0]["is_default"] == false);
    }
}
