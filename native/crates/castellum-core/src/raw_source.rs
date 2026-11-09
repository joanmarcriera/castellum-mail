// Castellum - on-demand full RFC822 source fetch + local cache (S49).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// No RFC822 raw source is ever stored — `message_bodies` only keeps parsed
// text/HTML (see this plan's Assumption 1). "Message Source" and the .eml
// export path both need the exact wire bytes, so this module fetches them
// over IMAP on demand, the same shape `attachments.rs` uses for incoming
// attachment bytes:
//   * raw_source_cache_status_json: local-only, no network — is this
//     message's source already cached, and where.
//   * fetch_raw_source_json: blocking network I/O — selects the mailbox,
//     verifies UIDVALIDITY against the stored sync cursor (the same guard
//     `hydrate_message_body_with_provider` applies, sync.rs), fetches the
//     full raw message via MailProvider::fetch_bodies, and writes it to the
//     cache.
//
// Unlike the attachment cache there is no LRU eviction budget here (see
// this plan's Assumption 6) — one file per distinct message ever viewed,
// named deterministically so a repeat view overwrites in place.

use crate::imap_connect::connect_guarded_imap;
use crate::sync::ImapSyncConfig;
use crate::CoreError;
use castellum_db::Database;
use castellum_providers::{ImapConfig, MailProvider};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The on-disk directory holding every cached raw-source file.
pub fn cache_dir(profile_path: &Path) -> PathBuf {
    profile_path.join("raw-source-cache")
}

/// The deterministic path a message's cached raw source lives at (whether
/// or not the file currently exists), named by a hash of the message id so
/// a repeat fetch of the same message overwrites the same file.
pub fn cached_file_path(profile_path: &Path, message_id: &str) -> PathBuf {
    let digest = Sha256::digest(message_id.as_bytes());
    cache_dir(profile_path).join(format!("{}.eml", crate::hex_lower(&digest)))
}

/// JSON result shape — identical to `attachments::AttachmentDownloadResult`
/// (same fields, same status vocabulary), so the Swift side decodes both
/// into the one existing DTO. Kept as this module's own type (rather than
/// importing attachments' type) so the two capabilities can evolve
/// independently, matching every other module's own small result struct
/// (HydrationReport, ReplayReport, ImportEmlResult, ...).
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RawSourceFetchResult {
    /// "cached" | "not_cached" | "downloaded" | "failed".
    pub status: String,
    pub local_path: Option<String>,
    pub error: Option<String>,
}

fn failed(reason: &str) -> RawSourceFetchResult {
    RawSourceFetchResult {
        status: "failed".into(),
        local_path: None,
        error: Some(reason.into()),
    }
}

fn to_json(result: &RawSourceFetchResult) -> Result<String, CoreError> {
    serde_json::to_string(result).map_err(|e| CoreError::Sync(format!("serialize: {e}")))
}

fn db_err<E: std::fmt::Display>(e: E) -> CoreError {
    CoreError::Sync(format!("db: {e}"))
}

/// Local-only: is this message's raw source already cached, and where.
/// Never opens a network connection.
pub fn raw_source_cache_status_json(
    db: &Database,
    profile_path: &Path,
    message_id: &str,
) -> Result<String, CoreError> {
    let _ = db; // symmetry with attachment_cache_status_json's signature; no DB lookup needed — presence is a pure filesystem check.
    let path = cached_file_path(profile_path, message_id);
    let result = if path.exists() {
        RawSourceFetchResult {
            status: "cached".into(),
            local_path: Some(path.to_string_lossy().into_owned()),
            error: None,
        }
    } else {
        RawSourceFetchResult {
            status: "not_cached".into(),
            local_path: None,
            error: None,
        }
    };
    to_json(&result)
}

/// Fetch the message's full raw source over IMAP, cache it, return the
/// result as JSON. Blocking network I/O unless already cached.
/// `config_json` carries account credentials (same `ImapSyncConfig` shape
/// `hydrate_message_body_json`/`download_attachment_json` already use).
pub fn fetch_raw_source_json(
    db: &Database,
    profile_path: &Path,
    config_json: &str,
    message_id: &str,
) -> Result<String, CoreError> {
    let cached_path = cached_file_path(profile_path, message_id);
    if cached_path.exists() {
        return to_json(&RawSourceFetchResult {
            status: "cached".into(),
            local_path: Some(cached_path.to_string_lossy().into_owned()),
            error: None,
        });
    }

    let cfg: ImapSyncConfig = serde_json::from_str(config_json)
        .map_err(|e| CoreError::Sync(format!("bad config json: {e}")))?;
    let Some(detail) = db.get_message_detail(message_id).map_err(db_err)? else {
        return to_json(&failed("message not found"));
    };
    if detail.header.account_id != cfg.account_id {
        return to_json(&failed("message does not belong to configured account"));
    }
    let Some((mailbox_path, uid)) = db.message_imap_coords(message_id).map_err(db_err)? else {
        return to_json(&failed("message has no provider uid"));
    };

    let mut provider = match connect_guarded_imap(&ImapConfig {
        host: cfg.host.clone(),
        port: cfg.port,
        username: cfg.username.clone(),
        password: cfg.password.clone(),
        security: castellum_providers::TransportSecurity::resolve(
            cfg.imap_security.as_deref(),
            cfg.use_tls,
        ),
        allow_invalid_certs: cfg.allow_invalid_certs,
    }) {
        Ok(p) => p,
        Err(e) => return to_json(&failed(&e.to_string())),
    };
    let outcome = fetch_with_provider(
        db,
        profile_path,
        &cfg.account_id,
        &detail.header.mailbox_id,
        &mailbox_path,
        uid,
        &mut provider,
    );
    let _ = provider.logout();
    to_json(&outcome?)
}

/// The provider-facing half of a fetch, given an already-connected
/// `provider`. Separated for unit testing with a mock provider.
fn fetch_with_provider(
    db: &Database,
    profile_path: &Path,
    account_id: &str,
    mailbox_id: &str,
    mailbox_path: &str,
    uid: i64,
    provider: &mut dyn MailProvider,
) -> Result<RawSourceFetchResult, CoreError> {
    let selected = match provider.select(mailbox_path) {
        Ok(s) => s,
        Err(e) => return Ok(failed(&e.to_string())),
    };
    let expected_uid_validity = db
        .get_sync_cursor(account_id, mailbox_id)
        .map_err(db_err)?
        .and_then(|c| c.uid_validity);
    if selected.uid_validity <= 0
        || expected_uid_validity.is_none_or(|expected| expected != selected.uid_validity)
    {
        return Ok(failed(
            "mailbox UIDVALIDITY changed; sync the mailbox before viewing this message's source",
        ));
    }

    let messages = match provider.fetch_bodies(&[uid]) {
        Ok(m) => m,
        Err(e) => return Ok(failed(&e.to_string())),
    };
    let Some(rm) = messages.into_iter().find(|m| m.uid == uid) else {
        return Ok(failed(&format!(
            "provider did not return a body for uid {uid}"
        )));
    };

    let path = store_raw_source(
        profile_path,
        &cache_key(account_id, mailbox_id, uid),
        &rm.raw,
    )?;
    Ok(RawSourceFetchResult {
        status: "downloaded".into(),
        local_path: Some(path.to_string_lossy().into_owned()),
        error: None,
    })
}

/// The message id this fetch is keyed on, reconstructed from its coords —
/// matches `NewMessage::id`'s own `"{mailbox_id}:{uid}"` shape (`sync.rs`),
/// so a later call from `fetch_raw_source_json` (which already has the real
/// `message_id` from the DB) and this internal provider-facing path always
/// agree on the same cache file.
fn cache_key(_account_id: &str, mailbox_id: &str, uid: i64) -> String {
    format!("{mailbox_id}:{uid}")
}

/// Write `raw` atomically into the cache under `message_id`'s deterministic
/// path (write-then-rename, matching the attachment cache's own atomicity
/// comment) and return that path.
fn store_raw_source(
    profile_path: &Path,
    message_id: &str,
    raw: &[u8],
) -> Result<PathBuf, CoreError> {
    let dir = cache_dir(profile_path);
    std::fs::create_dir_all(&dir).map_err(|_| CoreError::Io)?;
    let dest = cached_file_path(profile_path, message_id);
    // Unique temp name so concurrent fetches of one message never collide (#3365).
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dest.with_extension(format!("eml.tmp-{}-{seq}", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(raw)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &dest)
    };
    if write().is_err() {
        let _ = std::fs::remove_file(&tmp);
        return Err(CoreError::Io);
    }
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use castellum_models::{Account, Address, Mailbox, NewMessage, SyncCursor};
    use castellum_providers::{MailboxState, ProviderError, RemoteMailbox, RemoteMessage};

    fn temp_db_and_profile(tag: &str) -> (Database, tempfile::TempDir) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("castellum-rawsource-test-{tag}-"))
            .tempdir()
            .expect("tempdir");
        let db = Database::open(&dir.path().join("castellum.db")).expect("open db");
        (db, dir)
    }

    fn seed_message(db: &Database) {
        db.upsert_account(&Account {
            id: "acc1".into(),
            email: "test@local.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.upsert_mailbox(&Mailbox {
            id: "acc1:INBOX".into(),
            account_id: "acc1".into(),
            path: "INBOX".into(),
            name: "INBOX".into(),
            role: Some("inbox".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.set_sync_cursor(
            "acc1",
            "acc1:INBOX",
            &SyncCursor {
                uid_validity: Some(1),
                uid_next: Some(2),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
        db.upsert_message(&NewMessage {
            id: "acc1:INBOX:1".into(),
            account_id: "acc1".into(),
            mailbox_id: "acc1:INBOX".into(),
            provider_uid: Some(1),
            rfc822_message_id: Some("m1@x".into()),
            subject: Some("Hi".into()),
            from_name: None,
            from_address: Some("a@x".into()),
            sent_at: Some(1000),
            snippet: Some("hi".into()),
            size_bytes: Some(4),
            has_attachments: false,
            attachments: None,
            is_read: false,
            is_starred: false,
            addresses: vec![Address {
                kind: "from".into(),
                name: None,
                address: "a@x".into(),
            }],
            text_body: Some("hi".into()),
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: vec![],
        })
        .unwrap();
    }

    struct MockProvider {
        uid_validity: i64,
        select_fails: bool,
        bodies: Vec<RemoteMessage>,
    }

    impl MailProvider for MockProvider {
        fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
            Ok(vec![])
        }
        fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
            if self.select_fails {
                return Err(ProviderError::Network("connection reset".into()));
            }
            Ok(MailboxState {
                uid_validity: self.uid_validity,
                uid_next: 100,
                exists: 1,
            })
        }
        fn fetch_new(&mut self, _after: Option<i64>) -> Result<Vec<RemoteMessage>, ProviderError> {
            Ok(vec![])
        }
        fn fetch_bodies(&mut self, _uids: &[i64]) -> Result<Vec<RemoteMessage>, ProviderError> {
            Ok(self.bodies.clone())
        }
        fn logout(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    #[test]
    fn cache_status_is_not_cached_before_any_fetch() {
        let (db, dir) = temp_db_and_profile("status");
        seed_message(&db);
        let json = raw_source_cache_status_json(&db, dir.path(), "acc1:INBOX:1").unwrap();
        let result: RawSourceFetchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result.status, "not_cached");
    }

    #[test]
    fn fetch_with_provider_writes_the_cache_file_and_reports_downloaded() {
        let (db, dir) = temp_db_and_profile("fetch");
        seed_message(&db);
        let mut provider = MockProvider {
            uid_validity: 1,
            select_fails: false,
            bodies: vec![RemoteMessage {
                uid: 1,
                raw: b"From: a@x\r\nSubject: Hi\r\n\r\nhi\r\n".to_vec(),
                is_seen: false,
                is_flagged: false,
            }],
        };
        let result = fetch_with_provider(
            &db,
            dir.path(),
            "acc1",
            "acc1:INBOX",
            "INBOX",
            1,
            &mut provider,
        )
        .unwrap();
        assert_eq!(result.status, "downloaded");
        let path = result.local_path.expect("local_path present");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"From: a@x\r\nSubject: Hi\r\n\r\nhi\r\n"
        );

        // A second status check (no network) now reports cached at the SAME path.
        let status_json = raw_source_cache_status_json(&db, dir.path(), "acc1:INBOX:1").unwrap();
        let status: RawSourceFetchResult = serde_json::from_str(&status_json).unwrap();
        assert_eq!(status.status, "cached");
        assert_eq!(status.local_path.as_deref(), Some(path.as_str()));
    }

    #[test]
    fn fetch_with_provider_refuses_a_uidvalidity_mismatch() {
        let (db, dir) = temp_db_and_profile("uidval");
        seed_message(&db); // cursor uid_validity = 1
        let mut provider = MockProvider {
            uid_validity: 2, // server now reports a different UIDVALIDITY
            select_fails: false,
            bodies: vec![],
        };
        let result = fetch_with_provider(
            &db,
            dir.path(),
            "acc1",
            "acc1:INBOX",
            "INBOX",
            1,
            &mut provider,
        )
        .unwrap();
        assert_eq!(result.status, "failed");
        assert!(result.error.unwrap().contains("UIDVALIDITY"));
    }

    #[test]
    fn fetch_raw_source_json_refuses_a_message_from_another_account() {
        let (db, dir) = temp_db_and_profile("wrongacct");
        seed_message(&db);
        let config = serde_json::json!({
            "account_id": "someone-else",
            "email": "x@x", "host": "127.0.0.1", "port": 1,
            "username": "x", "password": "x",
        })
        .to_string();
        let json = fetch_raw_source_json(&db, dir.path(), &config, "acc1:INBOX:1").unwrap();
        let result: RawSourceFetchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result.status, "failed");
        assert!(result.error.unwrap().contains("does not belong"));
    }
}
