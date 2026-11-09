// Castellum - on-demand incoming attachment download + local cache (S32).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Two entry points, both FFI-facing (see castellum-ffi):
//   * attachment_cache_status_json: local-only, no network — "is this
//     attachment already on disk, and where". Cheap; called on message
//     selection and to poll after a download.
//   * download_attachment_json: blocking network I/O — fetches the one MIME
//     part via MailProvider::fetch_attachment_part, hashes it, writes it
//     atomically into the content-addressed cache, evicts LRU entries if
//     the cache exceeds budget, and returns the local path.
//
// The cache directory lives under the profile directory
// (<profile>/attachments-cache/<sha256-hex>), never inside the SQLite file
// itself — Rust owns this durable state end to end; Swift only ever reads
// the path it's handed back.

use crate::imap_connect::connect_guarded_imap;
use crate::sync::ImapSyncConfig;
use crate::CoreError;
use castellum_db::Database;
use castellum_providers::{ImapConfig, MailProvider};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Default cache budget: 500 MB. Chosen as a conservative desktop-app
/// default (a handful of large PDFs/videos, not a media library) —
/// revisit once real usage data exists; no UI to change it in this plan.
pub const DEFAULT_CACHE_BUDGET_BYTES: i64 = 500 * 1024 * 1024;

/// The on-disk directory holding every cached attachment file, named by its
/// content's sha256 hex digest.
pub fn cache_dir(profile_path: &Path) -> PathBuf {
    profile_path.join("attachments-cache")
}

/// The path a given content hash's bytes live at (whether or not the file
/// currently exists).
pub fn cached_file_path(profile_path: &Path, content_hash: &str) -> PathBuf {
    cache_dir(profile_path).join(content_hash)
}

/// Unlink the cached files of content hashes no `attachment_cache` row
/// references any more — what a sync pass reports in
/// `SyncReport::orphaned_attachment_hashes` after it removed a folder that
/// is gone from the server. The one place every production
/// caller of a pass goes through (`Profile::sync_imap_json` and the IDLE
/// resync), so no path can purge the rows and leave the bytes behind.
/// Best-effort like every other cache unlink here: the rows are already
/// gone, and a file that cannot be removed is only wasted space.
pub(crate) fn unlink_orphaned_files(profile_path: &Path, content_hashes: &[String]) {
    for hash in content_hashes {
        let _ = std::fs::remove_file(cached_file_path(profile_path, hash));
    }
}

/// JSON result shape for both FFI entry points below.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachmentDownloadResult {
    /// "cached" | "not_cached" | "downloaded" | "failed".
    pub status: String,
    pub local_path: Option<String>,
    pub error: Option<String>,
}

/// Local-only lookup: is (message_id, ordinal) already cached, and at what
/// path. Never opens a network connection. Touches the LRU clock on a hit.
pub fn attachment_cache_status_json(
    db: &Database,
    profile_path: &Path,
    message_id: &str,
    ordinal: i64,
) -> Result<String, CoreError> {
    let result = cache_status(db, profile_path, message_id, ordinal, now_epoch())?;
    to_json(&result)
}

fn cache_status(
    db: &Database,
    profile_path: &Path,
    message_id: &str,
    ordinal: i64,
    now: i64,
) -> Result<AttachmentDownloadResult, CoreError> {
    let Some(entry) = db
        .attachment_cache_entry(message_id, ordinal)
        .map_err(db_err)?
    else {
        return Ok(AttachmentDownloadResult {
            status: "not_cached".into(),
            local_path: None,
            error: None,
        });
    };
    let path = cached_file_path(profile_path, &entry.content_hash);
    if !path.exists() {
        // DB says cached but the file is gone (manual deletion, disk
        // cleanup tool) — treat as not cached rather than lying about a
        // path that won't open. A future download call will re-fetch.
        return Ok(AttachmentDownloadResult {
            status: "not_cached".into(),
            local_path: None,
            error: None,
        });
    }
    db.touch_attachment_cache_access(message_id, ordinal, now)
        .map_err(db_err)?;
    Ok(AttachmentDownloadResult {
        status: "cached".into(),
        local_path: Some(path.to_string_lossy().into_owned()),
        error: None,
    })
}

/// Fetch the attachment part over IMAP, cache it, return the download
/// result as JSON. Blocking network I/O. `config_json` carries account
/// credentials (same `ImapSyncConfig` shape `hydrate_message_body_json`
/// already uses). A request that cannot succeed (unknown message, another
/// account's message, unknown ordinal, no provider uid) is refused BEFORE any
/// connection is opened; every refusal is a `"failed"` result, not an `Err`.
///
/// The connection work lives in [`download_with_provider`], which takes the
/// provider as a parameter so it is unit-testable with a mock.
pub fn download_attachment_json(
    db: &Database,
    profile_path: &Path,
    config_json: &str,
    message_id: &str,
    ordinal: i64,
) -> Result<String, CoreError> {
    // Already cached? Don't touch the network (or even parse the config).
    let cached = cache_status(db, profile_path, message_id, ordinal, now_epoch())?;
    if cached.status == "cached" {
        return to_json(&cached);
    }

    let cfg: ImapSyncConfig = serde_json::from_str(config_json)
        .map_err(|e| CoreError::Sync(format!("bad config json: {e}")))?;
    let Some(detail) = db.get_message_detail(message_id).map_err(db_err)? else {
        return to_json(&failed("message not found"));
    };
    if detail.header.account_id != cfg.account_id {
        return to_json(&failed("message does not belong to configured account"));
    }
    if !detail.attachments.iter().any(|a| a.ordinal == ordinal) {
        return to_json(&failed("attachment not found"));
    }
    if db
        .message_imap_coords(message_id)
        .map_err(db_err)?
        .is_none()
    {
        return to_json(&failed("message has no provider uid"));
    }

    // Guarded through the connect itself (LOGIN/CAPABILITY panic sites).
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
    let outcome = download_with_provider(
        db,
        profile_path,
        &cfg.account_id,
        message_id,
        ordinal,
        &mut provider,
        &cfg.host,
    );
    let _ = provider.logout();
    to_json(&outcome?)
}

/// Copy every attachment of `message_id` into `draft_id` (used by Forward).
/// Downloads any attachment that is not already cached (same path as
/// [`download_attachment_json`]; `config_json` carries the SOURCE message's
/// account credentials), then adds each as a draft attachment, preserving
/// `is_inline`/`content_id` so a forwarded HTML message's inline images keep
/// their original `cid:` references (built into real MIME by
/// `build_rfc822`). A single attachment that cannot be fetched is skipped
/// rather than aborting the rest; the caller sees the shortfall by comparing
/// the returned list's length with the source message's attachment count.
/// Returns the draft's refreshed attachment list as JSON.
pub fn copy_message_attachments_to_draft_json(
    db: &Database,
    profile_path: &Path,
    config_json: &str,
    message_id: &str,
    draft_id: i64,
) -> Result<String, CoreError> {
    let descriptors = db.list_message_attachments(message_id).map_err(db_err)?;
    for descriptor in descriptors {
        let mut result = cache_status(
            db,
            profile_path,
            message_id,
            descriptor.ordinal,
            now_epoch(),
        )?;
        if result.status != "cached" {
            let json = download_attachment_json(
                db,
                profile_path,
                config_json,
                message_id,
                descriptor.ordinal,
            )?;
            result = serde_json::from_str(&json)
                .map_err(|e| CoreError::Sync(format!("bad download result: {e}")))?;
        }
        let Some(path) = result.local_path else {
            continue;
        };
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let filename = descriptor
            .filename
            .unwrap_or_else(|| "attachment".to_string());
        let content_type = descriptor
            .content_type
            .unwrap_or_else(|| "application/octet-stream".to_string());
        db.add_draft_attachment(
            draft_id,
            &filename,
            &content_type,
            descriptor.is_inline,
            descriptor.content_id.as_deref(),
            &bytes,
        )
        .map_err(db_err)?;
    }
    let list = db.list_draft_attachments(draft_id).map_err(db_err)?;
    serde_json::to_string(&list).map_err(|e| CoreError::Sync(format!("serialize: {e}")))
}

/// The provider-facing half of a download, given an already-connected
/// `provider`.
///
/// Selects the message's mailbox and checks its UIDVALIDITY against the
/// account's sync cursor before trusting `uid` — the exact same guard
/// `hydrate_message_body_with_provider` applies before its own
/// `fetch_bodies` call (`native/crates/castellum-core/src/sync.rs`).
/// IMAP UIDs are only meaningful within a SELECTed mailbox, and are only
/// safe to reuse across syncs while UIDVALIDITY is unchanged — skipping
/// this would either fail the FETCH outright against a real server (no
/// mailbox selected) or, worse, silently download a *different* message's
/// bytes under a stale UID after the mailbox's UIDVALIDITY changed.
///
/// A row persisted before migration 0021 has no section address; instead of
/// asking the user to do something the UI cannot (re-opening a hydrated
/// message never re-hydrates it) the message is re-fetched and re-parsed here,
/// which fills in the section and transfer encoding.
///
/// `BODY[<section>]` returns the part still in its Content-Transfer-Encoding,
/// so the bytes are decoded (base64 / quoted-printable) before they are
/// cached; data that is malformed for its declared encoding is refused
/// rather than cached as garbage.
fn download_with_provider(
    db: &Database,
    profile_path: &Path,
    account_id: &str,
    message_id: &str,
    ordinal: i64,
    provider: &mut dyn MailProvider,
    provider_host: &str,
) -> Result<AttachmentDownloadResult, CoreError> {
    let Some(detail) = db.get_message_detail(message_id).map_err(db_err)? else {
        return Ok(failed("message not found"));
    };
    let Some((mailbox_path, uid)) = db.message_imap_coords(message_id).map_err(db_err)? else {
        return Ok(failed("message has no provider uid"));
    };
    let mailbox_id = detail.header.mailbox_id.clone();

    let selected = match provider.select(&mailbox_path) {
        Ok(s) => s,
        Err(e) => return Ok(failed(&e.to_string())),
    };
    let expected_uid_validity = db
        .get_sync_cursor(account_id, &mailbox_id)
        .map_err(db_err)?
        .and_then(|c| c.uid_validity);
    if selected.uid_validity <= 0
        || expected_uid_validity.is_none_or(|expected| expected != selected.uid_validity)
    {
        return Ok(failed(
            "mailbox UIDVALIDITY changed; sync the mailbox before downloading this attachment",
        ));
    }

    let Some(mut descriptor) = detail
        .attachments
        .into_iter()
        .find(|a| a.ordinal == ordinal)
    else {
        return Ok(failed("attachment not found"));
    };
    if descriptor.section.is_empty() {
        if let Err(reason) = rehydrate_message(
            db,
            account_id,
            &mailbox_id,
            message_id,
            uid,
            provider,
            provider_host,
        ) {
            return Ok(failed(&reason));
        }
        let refreshed = db.list_message_attachments(message_id).map_err(db_err)?;
        descriptor = match refreshed.into_iter().find(|a| a.ordinal == ordinal) {
            Some(d) if !d.section.is_empty() => d,
            _ => return Ok(failed("attachment is no longer part of this message")),
        };
    }

    let raw = match provider.fetch_attachment_part(uid, &descriptor.section) {
        Ok(b) => b,
        Err(e) => return Ok(failed(&e.to_string())),
    };
    let Some(bytes) =
        castellum_mail::decode_transfer_encoding(&raw, descriptor.transfer_encoding.as_deref())
    else {
        return Ok(failed(
            "attachment data is malformed for its declared transfer encoding",
        ));
    };

    let path = store_downloaded(
        db,
        profile_path,
        message_id,
        ordinal,
        &bytes,
        DEFAULT_CACHE_BUDGET_BYTES,
    )?;
    Ok(AttachmentDownloadResult {
        status: "downloaded".into(),
        local_path: Some(path.to_string_lossy().into_owned()),
        error: None,
    })
}

/// Re-fetch and re-parse one message so its attachment descriptors (section,
/// content id, transfer encoding) are current. Same fetch + upsert as the
/// body-on-open path — and, per S44, the same List-Unsubscribe capture:
/// this is a genuine `new_message_from_raw` call site with a full RFC822
/// body, so it persists the header targets too (deviation from the S44 plan,
/// which only named `sync.rs`'s call sites; this one turned up on the
/// required `grep -rn "new_message_from_raw("` sweep). The error is a short
/// user-facing reason (never message content).
fn rehydrate_message(
    db: &Database,
    account_id: &str,
    mailbox_id: &str,
    message_id: &str,
    uid: i64,
    provider: &mut dyn MailProvider,
    provider_host: &str,
) -> Result<(), String> {
    let messages = provider.fetch_bodies(&[uid]).map_err(|e| e.to_string())?;
    let Some(rm) = messages.into_iter().find(|m| m.uid == uid) else {
        return Err("the server no longer has this message".into());
    };
    let (refreshed, extras) =
        crate::sync::new_message_from_raw(account_id, mailbox_id, rm, provider_host);
    db.upsert_message(&refreshed)
        .map_err(|e| format!("db: {e}"))?;
    db.set_unsubscribe_targets(
        message_id,
        extras.list_unsubscribe_mailto.as_deref(),
        extras.list_unsubscribe_url.as_deref(),
        extras.list_unsubscribe_one_click,
    )
    .map_err(|e| format!("db: {e}"))?;
    db.upsert_message_trust(&refreshed.id, &extras.trust)
        .map_err(|e| format!("db: {e}"))?;
    // MM-1c: a re-hydrated body re-captures headers (replace, never merge).
    crate::sync::persist_message_headers(db, &refreshed.id, &extras);
    Ok(())
}

fn failed(reason: &str) -> AttachmentDownloadResult {
    AttachmentDownloadResult {
        status: "failed".into(),
        local_path: None,
        error: Some(reason.to_string()),
    }
}

fn to_json(result: &AttachmentDownloadResult) -> Result<String, CoreError> {
    serde_json::to_string(result).map_err(|e| CoreError::Sync(format!("serialize: {e}")))
}

/// Write `bytes` into the content-addressed cache (atomically: a temp file
/// in the same directory, then `rename`, so a crash mid-write can never
/// leave a half-written file at the final hash path), upsert the DB row at
/// the current wall-clock time, then evict LRU entries if over budget.
/// Returns the final on-disk path.
fn store_downloaded(
    db: &Database,
    profile_path: &Path,
    message_id: &str,
    ordinal: i64,
    bytes: &[u8],
    budget_bytes: i64,
) -> Result<PathBuf, CoreError> {
    store_downloaded_at(
        db,
        profile_path,
        message_id,
        ordinal,
        bytes,
        budget_bytes,
        now_epoch(),
    )
}

/// Process-wide counter giving every temp write its own file name.
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn store_downloaded_at(
    db: &Database,
    profile_path: &Path,
    message_id: &str,
    ordinal: i64,
    bytes: &[u8],
    budget_bytes: i64,
    now: i64,
) -> Result<PathBuf, CoreError> {
    let hash = crate::hex_lower(&Sha256::digest(bytes));
    let dir = cache_dir(profile_path);
    std::fs::create_dir_all(&dir).map_err(|_| CoreError::Io)?;
    let final_path = cached_file_path(profile_path, &hash);
    if !final_path.exists() {
        // Unique per write: concurrent downloads (Save All) must never share
        // a temp file, or one rename moves another download's bytes over a
        // hash-named path (#3365). Same-hash writers each rename identical
        // content, so a lost race is harmless.
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp_path = dir.join(format!(".tmp-{}-{}", std::process::id(), seq));
        let write = || -> std::io::Result<()> {
            let mut f = std::fs::File::create(&tmp_path)?;
            f.write_all(bytes)?;
            f.sync_all()?;
            std::fs::rename(&tmp_path, &final_path)
        };
        if write().is_err() {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(CoreError::Io);
        }
    }
    db.upsert_attachment_cache_entry(message_id, ordinal, &hash, bytes.len() as i64, now)
        .map_err(db_err)?;
    evict_if_over_budget(db, profile_path, budget_bytes, &hash)?;
    // Defence in depth: never report "downloaded" for a file that is not there.
    if !final_path.exists() {
        return Err(CoreError::Io);
    }
    Ok(final_path)
}

/// Delete least-recently-used cached hashes (DB rows first, then the file —
/// see `Database::delete_attachment_cache_hash`'s doc comment for why that
/// order is crash-safe) until the total cache size is at or under budget.
///
/// `keep_hash` (the file that was just stored) is never evicted: an attachment
/// bigger than the whole budget would otherwise be deleted the moment it was
/// downloaded while still being reported as downloaded (audit #3442). The
/// cache may therefore exceed the budget by that one file, until a later
/// download evicts it by normal LRU order.
fn evict_if_over_budget(
    db: &Database,
    profile_path: &Path,
    budget_bytes: i64,
    keep_hash: &str,
) -> Result<(), CoreError> {
    let mut total = db.total_attachment_cache_bytes().map_err(db_err)?;
    if total <= budget_bytes {
        return Ok(());
    }
    for usage in db.attachment_cache_hashes_by_lru().map_err(db_err)? {
        if total <= budget_bytes {
            break;
        }
        if usage.content_hash == keep_hash {
            continue;
        }
        db.delete_attachment_cache_hash(&usage.content_hash)
            .map_err(db_err)?;
        let _ = std::fs::remove_file(cached_file_path(profile_path, &usage.content_hash));
        total -= usage.size_bytes;
    }
    Ok(())
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn db_err<E: std::fmt::Display>(e: E) -> CoreError {
    CoreError::Sync(format!("db: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use castellum_models::{Account, Address, AttachmentDescriptor, NewMessage, SyncCursor};
    use castellum_providers::{MailboxState, ProviderError, RemoteMailbox, RemoteMessage};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_db_and_profile() -> (Database, tempfile::TempDir) {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = tempfile::Builder::new()
            .prefix(&format!(
                "castellum-attachments-test-{}-{n}-",
                std::process::id()
            ))
            .tempdir()
            .expect("tempdir");
        let db = Database::open(&dir.path().join("castellum.db")).expect("open db");
        (db, dir)
    }

    fn seed_message_with_attachment(db: &Database, message_id: &str) {
        seed_message(db, message_id, 7, "2", Some("base64"));
    }

    fn seed_message_with_attachment_uid(db: &Database, message_id: &str, provider_uid: i64) {
        seed_message(db, message_id, provider_uid, "2", Some("base64"));
    }

    /// One message in acc1:INBOX carrying one attachment (ordinal 0) with the
    /// given section address and transfer encoding.
    fn seed_message(
        db: &Database,
        message_id: &str,
        provider_uid: i64,
        section: &str,
        transfer_encoding: Option<&str>,
    ) {
        seed_message_inline(
            db,
            message_id,
            provider_uid,
            section,
            transfer_encoding,
            None,
        );
    }

    /// As [`seed_message`], optionally making the attachment an inline part
    /// with the given Content-ID.
    fn seed_message_inline(
        db: &Database,
        message_id: &str,
        provider_uid: i64,
        section: &str,
        transfer_encoding: Option<&str>,
        inline_cid: Option<&str>,
    ) {
        db.upsert_account(&Account {
            id: "acc1".into(),
            email: "alice@example.com".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.upsert_mailbox(&castellum_models::Mailbox {
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
        db.upsert_message(&NewMessage {
            id: message_id.into(),
            account_id: "acc1".into(),
            mailbox_id: "acc1:INBOX".into(),
            provider_uid: Some(provider_uid),
            rfc822_message_id: None,
            subject: None,
            from_name: None,
            from_address: None,
            sent_at: None,
            snippet: None,
            size_bytes: None,
            has_attachments: true,
            attachments: Some(vec![AttachmentDescriptor {
                ordinal: 0,
                filename: Some("report.pdf".into()),
                content_type: Some("application/pdf".into()),
                size_bytes: Some(5),
                section: section.into(),
                content_id: inline_cid.map(str::to_owned),
                is_inline: inline_cid.is_some(),
                transfer_encoding: transfer_encoding.map(str::to_owned),
            }]),
            is_read: false,
            is_starred: false,
            addresses: vec![Address {
                kind: "from".into(),
                name: None,
                address: "bob@example.com".into(),
            }],
            text_body: None,
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: vec![],
        })
        .unwrap();
    }

    fn set_uid_validity(db: &Database, validity: Option<i64>) {
        db.set_sync_cursor(
            "acc1",
            "acc1:INBOX",
            &SyncCursor {
                uid_validity: validity,
                uid_next: Some(100),
                highest_modseq: None,
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
    }

    /// A scripted provider: records every call and returns canned data.
    #[derive(Default)]
    struct MockProvider {
        uid_validity: i64,
        select_fails: bool,
        part: Vec<u8>,
        bodies: Vec<RemoteMessage>,
        calls: Vec<String>,
    }

    impl MailProvider for MockProvider {
        fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
            Ok(vec![])
        }
        fn select(&mut self, path: &str) -> Result<MailboxState, ProviderError> {
            self.calls.push(format!("select {path}"));
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
        fn fetch_bodies(&mut self, uids: &[i64]) -> Result<Vec<RemoteMessage>, ProviderError> {
            self.calls.push(format!("fetch_bodies {uids:?}"));
            Ok(self.bodies.clone())
        }
        fn fetch_attachment_part(
            &mut self,
            uid: i64,
            section: &str,
        ) -> Result<Vec<u8>, ProviderError> {
            self.calls.push(format!("fetch_part {uid} {section}"));
            Ok(self.part.clone())
        }
        fn logout(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    fn run(
        db: &Database,
        dir: &tempfile::TempDir,
        message_id: &str,
        provider: &mut MockProvider,
    ) -> AttachmentDownloadResult {
        download_with_provider(db, dir.path(), "acc1", message_id, 0, provider, "test-host")
            .unwrap()
    }

    #[test]
    fn cache_status_is_not_cached_before_any_download() {
        let (db, dir) = temp_db_and_profile();
        seed_message_with_attachment(&db, "acc1:m1");
        let json = attachment_cache_status_json(&db, dir.path(), "acc1:m1", 0).unwrap();
        let result: AttachmentDownloadResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result.status, "not_cached");
        assert_eq!(result.local_path, None);
    }

    fn seed_forward_draft(db: &Database) -> i64 {
        db.save_draft(&castellum_models::NewDraft {
            id: None,
            account_id: "acc1".into(),
            to: vec![],
            cc: vec![],
            bcc: vec![],
            subject: "Fwd: s".into(),
            body_text: "".into(),
            body_html: None,
            from_email: None,
            from_name: None,
            in_reply_to: None,
            references_header: None,
        })
        .unwrap()
    }

    #[test]
    fn concurrent_downloads_in_one_second_never_cross_contaminate() {
        // #3365: Save All starts parallel downloads; the temp name used to be
        // pid+epoch-second, so same-second writers truncated each other.
        let (db, dir) = temp_db_and_profile();
        const N: usize = 8;
        for i in 0..N {
            seed_message_with_attachment_uid(&db, &format!("acc1:m{i}"), 100 + i as i64);
        }
        let payloads: Vec<Vec<u8>> = (0..N).map(|i| vec![b'A' + i as u8; 4 << 20]).collect();
        let barrier = std::sync::Barrier::new(N);
        let paths: Vec<PathBuf> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..N)
                .map(|i| {
                    let (db, dir, payloads, barrier) = (&db, &dir, &payloads, &barrier);
                    s.spawn(move || {
                        barrier.wait();
                        store_downloaded_at(
                            db,
                            dir.path(),
                            &format!("acc1:m{i}"),
                            0,
                            &payloads[i],
                            i64::MAX,
                            1_000,
                        )
                        .expect("every concurrent store succeeds")
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for (i, p) in paths.iter().enumerate() {
            assert_eq!(&std::fs::read(p).unwrap(), &payloads[i], "file {i} content");
        }
        let leftovers: Vec<_> = std::fs::read_dir(cache_dir(dir.path()))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "no temp files left behind");
    }

    #[test]
    fn copy_message_attachments_to_draft_copies_cached_bytes() {
        let (db, dir) = temp_db_and_profile();
        seed_message_with_attachment(&db, "acc1:m1");
        store_downloaded_at(&db, dir.path(), "acc1:m1", 0, b"hello world", 1_000, 1_000).unwrap();
        let draft_id = seed_forward_draft(&db);

        let json =
            copy_message_attachments_to_draft_json(&db, dir.path(), "{}", "acc1:m1", draft_id)
                .unwrap();
        let list: Vec<castellum_models::DraftAttachmentMeta> = serde_json::from_str(&json).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].filename, "report.pdf");
        assert_eq!(list[0].content_type, "application/pdf");
        assert_eq!(list[0].size_bytes, 11);
        assert!(!list[0].is_inline);
        assert_eq!(
            db.draft_attachment_bytes(list[0].id).unwrap().unwrap(),
            b"hello world"
        );
    }

    #[test]
    fn copy_message_attachments_preserves_inline_flag_and_content_id() {
        let (db, dir) = temp_db_and_profile();
        seed_message_inline(&db, "acc1:m1", 7, "2", Some("base64"), Some("logo123"));
        store_downloaded_at(&db, dir.path(), "acc1:m1", 0, b"png", 1_000, 1_000).unwrap();
        let draft_id = seed_forward_draft(&db);

        let json =
            copy_message_attachments_to_draft_json(&db, dir.path(), "{}", "acc1:m1", draft_id)
                .unwrap();
        let list: Vec<castellum_models::DraftAttachmentMeta> = serde_json::from_str(&json).unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].is_inline);
        assert_eq!(list[0].content_id.as_deref(), Some("logo123"));
    }

    #[test]
    fn copy_skips_an_attachment_that_cannot_be_fetched() {
        let (db, dir) = temp_db_and_profile();
        // Not cached, and the config names a different account, so the
        // download is refused before any connection is opened.
        seed_message_with_attachment(&db, "acc1:m1");
        let draft_id = seed_forward_draft(&db);
        let cfg = r#"{"account_id":"other","email":"o@example.com","host":"127.0.0.1",
                      "port":1,"username":"u","password":"p"}"#;
        let json =
            copy_message_attachments_to_draft_json(&db, dir.path(), cfg, "acc1:m1", draft_id)
                .unwrap();
        assert_eq!(json, "[]");
    }

    #[test]
    fn store_downloaded_writes_content_addressed_file_and_reports_cached() {
        let (db, dir) = temp_db_and_profile();
        seed_message_with_attachment(&db, "acc1:m1");
        let path = store_downloaded(&db, dir.path(), "acc1:m1", 0, b"hello", 1_000).unwrap();
        assert!(path.exists());
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");

        let json = attachment_cache_status_json(&db, dir.path(), "acc1:m1", 0).unwrap();
        let result: AttachmentDownloadResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result.status, "cached");
        assert_eq!(result.local_path.as_deref(), Some(path.to_str().unwrap()));
    }

    #[test]
    fn store_downloaded_dedupes_identical_bytes_across_messages() {
        let (db, dir) = temp_db_and_profile();
        seed_message_with_attachment_uid(&db, "acc1:m1", 7);
        seed_message_with_attachment_uid(&db, "acc1:m2", 8);
        let path1 = store_downloaded(&db, dir.path(), "acc1:m1", 0, b"same bytes", 1_000).unwrap();
        let path2 = store_downloaded(&db, dir.path(), "acc1:m2", 0, b"same bytes", 1_000).unwrap();
        assert_eq!(path1, path2, "identical content hashes to the same file");
        let file_count = std::fs::read_dir(cache_dir(dir.path())).unwrap().count();
        assert_eq!(
            file_count, 1,
            "only one physical file for two identical attachments"
        );
    }

    #[test]
    fn eviction_removes_lru_entries_once_over_budget() {
        let (db, dir) = temp_db_and_profile();
        seed_message_with_attachment_uid(&db, "acc1:m1", 7);
        seed_message_with_attachment_uid(&db, "acc1:m2", 8);
        // Budget (15 bytes) holds one 10-byte entry but not two: the second
        // write pushes the total to 20, forcing eviction of the older (m1)
        // hash back down to 10.
        store_downloaded_at(&db, dir.path(), "acc1:m1", 0, b"AAAAAAAAAA", 15, 1_000).unwrap();
        store_downloaded_at(&db, dir.path(), "acc1:m2", 0, b"BBBBBBBBBB", 15, 2_000).unwrap();

        assert_eq!(
            db.total_attachment_cache_bytes().unwrap(),
            10,
            "one entry evicted to stay <= budget"
        );
        let json = attachment_cache_status_json(&db, dir.path(), "acc1:m1", 0).unwrap();
        let result: AttachmentDownloadResult = serde_json::from_str(&json).unwrap();
        assert_eq!(
            result.status, "not_cached",
            "the older (LRU) entry was evicted"
        );
        assert!(
            !cached_file_path(
                dir.path(),
                &crate::hex_lower(&Sha256::digest(b"AAAAAAAAAA"))
            )
            .exists(),
            "the evicted file is gone from disk too"
        );
        let json2 = attachment_cache_status_json(&db, dir.path(), "acc1:m2", 0).unwrap();
        let result2: AttachmentDownloadResult = serde_json::from_str(&json2).unwrap();
        assert_eq!(result2.status, "cached", "the newer entry survives");
    }

    #[test]
    fn a_cache_hit_refreshes_the_lru_clock_so_it_outlives_a_newer_write() {
        let (db, dir) = temp_db_and_profile();
        seed_message_with_attachment_uid(&db, "acc1:m1", 7);
        seed_message_with_attachment_uid(&db, "acc1:m2", 8);
        seed_message_with_attachment_uid(&db, "acc1:m3", 9);
        store_downloaded_at(&db, dir.path(), "acc1:m1", 0, b"AAAAAAAAAA", 25, 1_000).unwrap();
        store_downloaded_at(&db, dir.path(), "acc1:m2", 0, b"BBBBBBBBBB", 25, 2_000).unwrap();
        // m1 is READ at t=3000 (real clock via cache_status): m2 is now the LRU entry.
        cache_status(&db, dir.path(), "acc1:m1", 0, 3_000).unwrap();
        store_downloaded_at(&db, dir.path(), "acc1:m3", 0, b"CCCCCCCCCC", 25, 4_000).unwrap();

        let m1 = attachment_cache_status_json(&db, dir.path(), "acc1:m1", 0).unwrap();
        let m2 = attachment_cache_status_json(&db, dir.path(), "acc1:m2", 0).unwrap();
        let m3 = attachment_cache_status_json(&db, dir.path(), "acc1:m3", 0).unwrap();
        assert!(
            m1.contains("\"cached\""),
            "recently read entry survives: {m1}"
        );
        assert!(
            m2.contains("not_cached"),
            "least recently used entry evicted: {m2}"
        );
        assert!(m3.contains("\"cached\""), "the new write survives: {m3}");
    }

    #[test]
    fn an_attachment_bigger_than_the_whole_budget_is_kept_not_deleted_after_download() {
        // Audit #3442: it used to be evicted at once yet reported as downloaded.
        let (db, dir) = temp_db_and_profile();
        seed_message_with_attachment_uid(&db, "acc1:m1", 7);
        let path = store_downloaded_at(
            &db,
            dir.path(),
            "acc1:m1",
            0,
            b"AAAAAAAAAAAAAAAAAAAA",
            10,
            1_000,
        )
        .unwrap();
        assert!(path.exists(), "the oversized file is still on disk");
        let json = attachment_cache_status_json(&db, dir.path(), "acc1:m1", 0).unwrap();
        assert!(json.contains("\"cached\""), "{json}");
        // A later, normal download evicts it by LRU like any other entry.
        seed_message_with_attachment_uid(&db, "acc1:m2", 8);
        store_downloaded_at(&db, dir.path(), "acc1:m2", 0, b"BBBBBBBB", 10, 2_000).unwrap();
        assert!(
            !path.exists(),
            "evicted once a newer download needs the room"
        );
    }

    #[test]
    fn a_cached_row_whose_file_vanished_reads_as_not_cached() {
        let (db, dir) = temp_db_and_profile();
        seed_message_with_attachment(&db, "acc1:m1");
        let path = store_downloaded(&db, dir.path(), "acc1:m1", 0, b"hello", 1_000).unwrap();
        std::fs::remove_file(path).unwrap();
        let json = attachment_cache_status_json(&db, dir.path(), "acc1:m1", 0).unwrap();
        assert!(json.contains("not_cached"), "{json}");
    }

    // --- download_with_provider ------------------------------------------

    #[test]
    fn download_decodes_the_base64_part_and_caches_the_decoded_bytes() {
        let (db, dir) = temp_db_and_profile();
        seed_message(&db, "acc1:m1", 7, "2", Some("base64"));
        set_uid_validity(&db, Some(42));
        let mut provider = MockProvider {
            uid_validity: 42,
            part: b"aGVsbG8g\r\nd29ybGQ=\r\n".to_vec(),
            ..Default::default()
        };

        let result = run(&db, &dir, "acc1:m1", &mut provider);

        assert_eq!(result.status, "downloaded", "{result:?}");
        let path = result.local_path.expect("path");
        assert_eq!(std::fs::read(path).unwrap(), b"hello world");
        assert_eq!(
            provider.calls,
            vec!["select INBOX".to_string(), "fetch_part 7 2".to_string()],
            "select first, then exactly one part fetch, no full-body fetch"
        );
    }

    #[test]
    fn download_passes_identity_encodings_through_unchanged() {
        let (db, dir) = temp_db_and_profile();
        seed_message(&db, "acc1:m1", 7, "1", Some("7bit"));
        set_uid_validity(&db, Some(42));
        let mut provider = MockProvider {
            uid_validity: 42,
            part: b"plain bytes".to_vec(),
            ..Default::default()
        };
        let result = run(&db, &dir, "acc1:m1", &mut provider);
        assert_eq!(
            std::fs::read(result.local_path.unwrap()).unwrap(),
            b"plain bytes"
        );
    }

    #[test]
    fn a_changed_uidvalidity_refuses_the_download_and_never_fetches_a_part() {
        let (db, dir) = temp_db_and_profile();
        seed_message(&db, "acc1:m1", 7, "2", Some("base64"));
        set_uid_validity(&db, Some(42));
        let mut provider = MockProvider {
            uid_validity: 43, // the server reset the mailbox
            part: b"aGk=".to_vec(),
            ..Default::default()
        };

        let result = run(&db, &dir, "acc1:m1", &mut provider);

        assert_eq!(result.status, "failed");
        assert!(result.error.unwrap().contains("UIDVALIDITY"));
        assert_eq!(provider.calls, vec!["select INBOX".to_string()]);
        assert_eq!(db.attachment_cache_entry("acc1:m1", 0).unwrap(), None);
    }

    #[test]
    fn a_missing_sync_cursor_is_treated_as_unverifiable_and_refused() {
        let (db, dir) = temp_db_and_profile();
        seed_message(&db, "acc1:m1", 7, "2", Some("base64"));
        // no set_uid_validity: the mailbox has never been synced
        let mut provider = MockProvider {
            uid_validity: 42,
            part: b"aGk=".to_vec(),
            ..Default::default()
        };
        let result = run(&db, &dir, "acc1:m1", &mut provider);
        assert_eq!(result.status, "failed");
        assert!(!provider.calls.iter().any(|c| c.starts_with("fetch_part")));
    }

    #[test]
    fn a_server_reporting_no_uidvalidity_is_refused() {
        let (db, dir) = temp_db_and_profile();
        seed_message(&db, "acc1:m1", 7, "2", Some("base64"));
        set_uid_validity(&db, Some(42));
        let mut provider = MockProvider {
            uid_validity: -1,
            ..Default::default()
        };
        assert_eq!(run(&db, &dir, "acc1:m1", &mut provider).status, "failed");
    }

    #[test]
    fn a_select_failure_is_a_typed_failure_not_an_error() {
        let (db, dir) = temp_db_and_profile();
        seed_message(&db, "acc1:m1", 7, "2", Some("base64"));
        set_uid_validity(&db, Some(42));
        let mut provider = MockProvider {
            select_fails: true,
            ..Default::default()
        };
        let result = run(&db, &dir, "acc1:m1", &mut provider);
        assert_eq!(result.status, "failed");
        assert!(result.error.unwrap().contains("connection reset"));
    }

    #[test]
    fn malformed_data_for_the_declared_encoding_is_refused_and_not_cached() {
        let (db, dir) = temp_db_and_profile();
        seed_message(&db, "acc1:m1", 7, "2", Some("base64"));
        set_uid_validity(&db, Some(42));
        let mut provider = MockProvider {
            uid_validity: 42,
            part: b"!!! not base64 !!!".to_vec(),
            ..Default::default()
        };
        let result = run(&db, &dir, "acc1:m1", &mut provider);
        assert_eq!(result.status, "failed");
        assert!(result.error.unwrap().contains("malformed"));
        assert_eq!(db.attachment_cache_entry("acc1:m1", 0).unwrap(), None);
        assert!(
            !cache_dir(dir.path()).exists()
                || std::fs::read_dir(cache_dir(dir.path())).unwrap().count() == 0
        );
    }

    #[test]
    fn a_pre_migration_row_without_a_section_is_rehydrated_then_downloaded() {
        let (db, dir) = temp_db_and_profile();
        // The row a pre-0021 profile has: hydrated body, but section '' and no
        // encoding. Its id matches what a re-parse produces so the upsert
        // replaces it in place.
        seed_message(&db, "acc1:INBOX:7", 7, "", None);
        set_uid_validity(&db, Some(42));
        let raw = "From: bob@example.com\r\n\
                   To: alice@example.com\r\n\
                   Subject: Report\r\n\
                   MIME-Version: 1.0\r\n\
                   Content-Type: multipart/mixed; boundary=\"B\"\r\n\
                   \r\n\
                   --B\r\n\
                   Content-Type: text/plain\r\n\
                   \r\n\
                   see attached\r\n\
                   --B\r\n\
                   Content-Type: application/pdf; name=\"report.pdf\"\r\n\
                   Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
                   Content-Transfer-Encoding: base64\r\n\
                   \r\n\
                   aGVsbG8=\r\n\
                   --B--\r\n";
        let mut provider = MockProvider {
            uid_validity: 42,
            part: b"aGVsbG8=".to_vec(),
            bodies: vec![RemoteMessage {
                uid: 7,
                raw: raw.as_bytes().to_vec(),
                is_seen: false,
                is_flagged: false,
            }],
            ..Default::default()
        };

        let result = run(&db, &dir, "acc1:INBOX:7", &mut provider);

        assert_eq!(result.status, "downloaded", "{result:?}");
        assert_eq!(std::fs::read(result.local_path.unwrap()).unwrap(), b"hello");
        assert_eq!(
            provider.calls,
            vec![
                "select INBOX".to_string(),
                "fetch_bodies [7]".to_string(),
                "fetch_part 7 2".to_string(),
            ],
            "re-hydrate once, then fetch the freshly computed section"
        );
        let stored = db.list_message_attachments("acc1:INBOX:7").unwrap();
        assert_eq!(stored[0].section, "2");
        assert_eq!(stored[0].transfer_encoding.as_deref(), Some("base64"));
    }

    #[test]
    fn rehydration_that_finds_no_such_attachment_fails_cleanly() {
        let (db, dir) = temp_db_and_profile();
        seed_message(&db, "acc1:INBOX:7", 7, "", None);
        set_uid_validity(&db, Some(42));
        // The server's copy has no attachments any more.
        let raw = b"From: bob@example.com\r\nSubject: x\r\n\r\njust text\r\n".to_vec();
        let mut provider = MockProvider {
            uid_validity: 42,
            bodies: vec![RemoteMessage {
                uid: 7,
                raw,
                is_seen: false,
                is_flagged: false,
            }],
            ..Default::default()
        };
        let result = run(&db, &dir, "acc1:INBOX:7", &mut provider);
        assert_eq!(result.status, "failed");
        assert!(!provider.calls.iter().any(|c| c.starts_with("fetch_part")));
    }

    #[test]
    fn rehydration_of_a_message_the_server_no_longer_has_fails_cleanly() {
        let (db, dir) = temp_db_and_profile();
        seed_message(&db, "acc1:INBOX:7", 7, "", None);
        set_uid_validity(&db, Some(42));
        let mut provider = MockProvider {
            uid_validity: 42,
            ..Default::default() // fetch_bodies returns nothing
        };
        let result = run(&db, &dir, "acc1:INBOX:7", &mut provider);
        assert_eq!(result.status, "failed");
        assert!(result.error.unwrap().contains("no longer has"));
    }

    // --- download_attachment_json (pre-network paths) ----------------------

    /// Config that would fail loudly if anything tried to connect: nothing
    /// listens on 127.0.0.1:1, so a connection attempt yields a network error
    /// string, which these tests assert never appears.
    fn unreachable_config(account_id: &str) -> String {
        serde_json::json!({
            "account_id": account_id,
            "email": "alice@example.com",
            "host": "127.0.0.1",
            "port": 1,
            "username": "alice",
            "password": "pw",
            "use_tls": false,
            "allow_invalid_certs": false,
            "mailboxes": ["INBOX"]
        })
        .to_string()
    }

    #[test]
    fn a_cached_attachment_is_served_without_parsing_config_or_connecting() {
        let (db, dir) = temp_db_and_profile();
        seed_message_with_attachment(&db, "acc1:m1");
        store_downloaded(&db, dir.path(), "acc1:m1", 0, b"hello", 1_000).unwrap();
        // A garbage config proves the cache short-circuits before any use of it.
        let json = download_attachment_json(&db, dir.path(), "not json", "acc1:m1", 0).unwrap();
        let result: AttachmentDownloadResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result.status, "cached");
    }

    #[test]
    fn a_message_from_another_account_is_refused_before_connecting() {
        let (db, dir) = temp_db_and_profile();
        seed_message_with_attachment(&db, "acc1:m1");
        let json = download_attachment_json(
            &db,
            dir.path(),
            &unreachable_config("someone-else"),
            "acc1:m1",
            0,
        )
        .unwrap();
        let result: AttachmentDownloadResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result.status, "failed");
        assert!(result.error.unwrap().contains("does not belong"));
    }

    #[test]
    fn an_unknown_message_or_ordinal_is_refused_before_connecting() {
        let (db, dir) = temp_db_and_profile();
        seed_message_with_attachment(&db, "acc1:m1");
        let cfg = unreachable_config("acc1");

        let missing = download_attachment_json(&db, dir.path(), &cfg, "acc1:nope", 0).unwrap();
        assert!(missing.contains("message not found"), "{missing}");

        let bad_ordinal = download_attachment_json(&db, dir.path(), &cfg, "acc1:m1", 5).unwrap();
        assert!(
            bad_ordinal.contains("attachment not found"),
            "{bad_ordinal}"
        );
    }

    #[test]
    fn a_bad_config_is_a_hard_error_not_a_failed_result() {
        let (db, dir) = temp_db_and_profile();
        seed_message_with_attachment(&db, "acc1:m1");
        assert!(download_attachment_json(&db, dir.path(), "not json", "acc1:m1", 0).is_err());
    }
}
