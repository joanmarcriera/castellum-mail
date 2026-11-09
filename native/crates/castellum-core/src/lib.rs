// Castellum - native core domain logic.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Slice 2 scope: a `Profile` now owns a real SQLite database opened under the
// profile directory (`<profile>/castellum.db`), with migrations applied on open.
// The API shape (open/close + a durable handle) is unchanged from slice 1 so the
// FFI boundary stays stable while the implementation grows.

use castellum_models::CastellumStatus;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub mod ai;
pub mod attachments;
pub mod autoconfig;
pub mod calendar_invites;
pub mod comm_health;
pub mod compose;
pub mod connection_test;
pub mod contacts;
pub mod diagnostics;
pub mod enrich;
pub mod envelope_repair;
pub mod graph;
pub mod identities;
pub mod idle;
mod imap_connect;
pub mod import_eml;
pub mod inline_images;
pub mod links;
pub mod pass_gate;
pub mod project_timeline;
pub mod projects;
pub mod query;
pub mod raw_source;
pub mod redact;
pub mod replay;
pub mod rules;
pub mod saved_views;
pub mod send;
pub mod smart_mailboxes;
pub mod suggestions;
pub mod sync;
pub mod sync_kpi;
pub mod sync_progress;
pub mod sync_timing;
pub mod unsubscribe;
pub mod work_items;

/// File name of the profile's SQLite database within the profile directory.
const DB_FILE: &str = "castellum.db";

/// Lowercase hex-encodes a byte slice (a `sha2::Sha256::digest()` output,
/// for the content-addressed attachment/raw-source caches).
///
/// sha2/digest 0.11 replaced `generic-array` with `hybrid-array`, which
/// dropped the `LowerHex`/`UpperHex` impls `{:x}` relied on — no upstream
/// hex formatting is available on the digest output any more, so this
/// hand-rolls it rather than pulling in the `hex` crate for one line.
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(s, "{b:02x}").expect("String write is infallible");
    }
    s
}

/// The native core version string reported across the FFI.
pub fn version() -> &'static str {
    castellum_models::VERSION
}

/// Errors from core operations. Each maps to a stable [`CastellumStatus`].
#[derive(Debug)]
pub enum CoreError {
    /// The requested path was empty or otherwise unusable.
    InvalidArgument,
    /// A filesystem or database error occurred while opening the profile.
    Io,
    /// A provider/network sync failure. The string carries diagnostics.
    Sync(String),
    /// A database/query error. The string carries diagnostics.
    Db(String),
    /// A queued operation whose payload/preconditions can never be replayed
    /// safely — retrying will not help (the classic case: a stale UIDVALIDITY
    /// snapshot, or a pre-snapshot legacy payload). Replay must classify this
    /// straight to the terminal `failed_permanent` status instead of the
    /// normal attempt-count ladder, so a destructive op never gets "lucky" on
    /// a later retry against a UID space that no longer means what it did.
    NonRetryable(String),
    /// The profile's applied schema is newer than this build supports (an
    /// older build opened a profile a newer one already migrated further —
    /// e.g. after a downgrade). The string is the full, non-technical,
    /// already-composed message (see `castellum_db::OpenError::SchemaTooNew`'s
    /// `Display`), prefixed with `castellum_db::SCHEMA_TOO_NEW_PREFIX` so the
    /// Swift launch path can classify it without a new ABI status code.
    SchemaTooNew(String),
    /// A migration failed to apply on open; the prior schema is unchanged
    /// and (for a non-empty profile) a pre-migration backup was taken. The
    /// string is the full message, prefixed with
    /// `castellum_db::MIGRATION_FAILED_PREFIX` — see
    /// `castellum_db::OpenError::MigrationFailed`'s `Display`.
    MigrationFailed(String),
}

impl CoreError {
    /// The ABI status this error maps to. `SchemaTooNew`/`MigrationFailed`
    /// deliberately reuse `Io` rather than adding new `CastellumStatus`
    /// values — same convention as the evaluation-mode refusal: the message
    /// (matched via its stable prefix) is the typed channel, not the status
    /// code, so no C header/enum change is needed to add a new refusal kind.
    pub fn status(&self) -> CastellumStatus {
        match self {
            CoreError::InvalidArgument | CoreError::NonRetryable(_) => {
                CastellumStatus::InvalidArgument
            }
            CoreError::Io => CastellumStatus::Io,
            CoreError::Sync(_) => CastellumStatus::Network,
            CoreError::Db(_) => CastellumStatus::Io,
            CoreError::SchemaTooNew(_) => CastellumStatus::Io,
            CoreError::MigrationFailed(_) => CastellumStatus::Io,
        }
    }
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CoreError::InvalidArgument => write!(f, "invalid argument"),
            CoreError::Io => write!(f, "io error"),
            CoreError::Sync(s)
            | CoreError::Db(s)
            | CoreError::NonRetryable(s)
            | CoreError::SchemaTooNew(s)
            | CoreError::MigrationFailed(s) => write!(f, "{s}"),
        }
    }
}

// Expose ai error helper for the AI run module.
impl From<castellum_db::DbError> for CoreError {
    fn from(e: castellum_db::DbError) -> Self {
        CoreError::Db(e.to_string())
    }
}

/// `Database::open`'s dedicated error type -> `CoreError`. Kept separate
/// from the `DbError` conversion above because `open` alone must distinguish
/// the two launch-blocking cases (see `castellum_db::OpenError`'s doc
/// comment) from a generic I/O failure.
impl From<castellum_db::OpenError> for CoreError {
    fn from(e: castellum_db::OpenError) -> Self {
        let text = e.to_string();
        match e {
            castellum_db::OpenError::Io(_) => CoreError::Db(text),
            castellum_db::OpenError::SchemaTooNew { .. } => CoreError::SchemaTooNew(text),
            castellum_db::OpenError::MigrationFailed { .. } => CoreError::MigrationFailed(text),
        }
    }
}

/// Cache files younger than this are never swept (#3364): they may belong
/// to a download whose DB row is about to be written.
pub const ORPHAN_SWEEP_MIN_AGE: std::time::Duration = std::time::Duration::from_secs(300);

/// An opened Castellum profile. Owns the profile directory and its SQLite
/// database. Later slices add provider/sync/AI state here.
pub struct Profile {
    path: PathBuf,
    db: castellum_db::Database,
    idle: idle::IdleSupervisor,
    /// Bumped each time an IDLE-thread resync pass finishes (see
    /// `Profile::idle_pass_epoch`). In-memory only.
    idle_pass_epoch: std::sync::atomic::AtomicU64,
}

impl Profile {
    /// The on-disk directory backing this profile.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The applied database schema version.
    pub fn schema_version(&self) -> Result<i64, CoreError> {
        self.db.schema_version().map_err(|_| CoreError::Io)
    }

    /// Whether the account may make changes to the mail server. Fail-closed:
    /// an unknown account or an account that has never left evaluation mode is
    /// read-only.
    pub fn account_write_enabled(&self, account_id: &str) -> Result<bool, CoreError> {
        self.db
            .account_write_enabled(account_id)
            .map_err(|e| CoreError::Db(e.to_string()))
    }

    /// Enable or disable writes for an account. Leaving evaluation mode
    /// (enabling writes) is the explicit human act that permits Castellum to
    /// change the mail server; callers must confirm the consequences first.
    pub fn set_account_write_enabled(
        &self,
        account_id: &str,
        enabled: bool,
    ) -> Result<(), CoreError> {
        self.db
            .set_account_write_enabled(account_id, enabled)
            .map_err(|e| CoreError::Db(e.to_string()))
    }

    /// Whether on-device CoreSpotlight indexing of message metadata is opted
    /// in. Fail-closed default `false` — see
    /// `Database::spotlight_indexing_enabled`. Profile-wide, not per-account.
    pub fn spotlight_indexing_enabled(&self) -> Result<bool, CoreError> {
        self.db
            .spotlight_indexing_enabled()
            .map_err(|e| CoreError::Db(e.to_string()))
    }

    /// Persist the CoreSpotlight indexing opt-in. The Swift side alone drives
    /// the resulting backfill / domain-scoped `CSSearchableIndex` removal —
    /// CoreSpotlight has no C ABI, so this method only persists the flag.
    pub fn set_spotlight_indexing_enabled(&self, enabled: bool) -> Result<(), CoreError> {
        self.db
            .set_spotlight_indexing_enabled(enabled)
            .map_err(|e| CoreError::Db(e.to_string()))
    }

    /// Permanently delete an account and every local mail record it owns
    /// (mailboxes, messages, sync cursors, queued operations, drafts). Errors
    /// when the account does not exist. Never touches the Keychain — the
    /// Swift `AccountStore` owns credential removal on its own side of the
    /// C ABI (see `native/crates/castellum-db/src/repo.rs::delete_account`).
    pub fn delete_account(&self, account_id: &str) -> Result<(), CoreError> {
        let (hashes, message_ids) = self
            .db
            .delete_account_collecting(account_id)
            .map_err(|e| CoreError::Db(e.to_string()))?;
        // #3364: the cached bytes outlive the cascaded rows, so unlink them
        // now (best-effort, after the commit: a leftover file is only wasted
        // disk). Shared hashes were excluded by the DB layer.
        for hash in hashes {
            let _ = std::fs::remove_file(attachments::cached_file_path(&self.path, &hash));
        }
        for id in message_ids {
            let _ = std::fs::remove_file(raw_source::cached_file_path(&self.path, &id));
        }
        // Expunged / reset messages leave files no per-path unlink can name.
        let _ = self.sweep_orphaned_caches(ORPHAN_SWEEP_MIN_AGE);
        Ok(())
    }

    /// Delete cache files nothing references any more (#3364): attachment
    /// files whose hash no `attachment_cache` row names, and raw-source
    /// `.eml` files whose message no longer exists. `min_age` spares files
    /// written that recently, so a write-then-insert in flight is never
    /// raced (a deleted-too-early file is only a re-download anyway).
    /// Returns how many files were removed.
    pub fn sweep_orphaned_caches(&self, min_age: std::time::Duration) -> Result<usize, CoreError> {
        let live_hashes = self
            .db
            .attachment_cache_live_hashes()
            .map_err(|e| CoreError::Db(e.to_string()))?;
        let live_raw: std::collections::HashSet<String> = self
            .db
            .all_message_ids()
            .map_err(|e| CoreError::Db(e.to_string()))?
            .iter()
            .map(|id| {
                raw_source::cached_file_path(&self.path, id)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .collect();
        let mut removed = 0;
        for (dir, live) in [
            (attachments::cache_dir(&self.path), &live_hashes),
            (raw_source::cache_dir(&self.path), &live_raw),
        ] {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                // In-flight temp files belong to a writer (see
                // `attachments::write_atomically`); age out only stale ones.
                let recent = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_none_or(|age| age < min_age);
                if recent || live.contains(&name) {
                    continue;
                }
                if std::fs::remove_file(entry.path()).is_ok() {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    /// Sync an IMAP account described by `config_json` into this profile,
    /// returning a JSON sync report. Read-only: never mutates remote state.
    ///
    /// Also persists a last-run KPI snapshot (`sync_kpi::write_last_sync_kpi`)
    /// so a later, out-of-band caller (the `castellum_last_sync_kpi` FFI call,
    /// and eventually a Diagnostics UI) can see what this sync cost without a
    /// sync in progress. `sync_timing` is turned on for the duration of this
    /// call ONLY when nothing else already enabled it on this thread (a bench
    /// harness or the CI perf-gate calls `sync_timing::enable()` itself
    /// around this same call and owns its own `take()`; this method must not
    /// steal those samples out from under it — see `sync_timing::snapshot`).
    /// Snapshot persistence is best-effort: an I/O error writing it is
    /// swallowed, exactly like the diagnostic writes elsewhere in this crate,
    /// and never changes the sync's own result.
    pub fn sync_imap_json(&self, config_json: &str) -> Result<String, CoreError> {
        let account_id = account_id_from_config(config_json);
        let owns_timing = !sync_timing::is_enabled();
        if owns_timing {
            sync_timing::enable();
        }
        let started = std::time::Instant::now();
        let result = sync::sync_imap_json(&self.db, config_json);
        let wall = started.elapsed();
        // A folder the pass removed because it is gone from the server
        // may have left cached attachment bytes no row
        // references any more. Best-effort unlink after the fact, exactly
        // like `reset_account` and `apply_mailbox_selection_json`. The IDLE
        // resync does the same through `sync::resync_on_live_connection`.
        attachments::unlink_orphaned_files(&self.path, &orphaned_attachment_hashes(&result));
        let timings = sync_timing::snapshot();
        if owns_timing {
            sync_timing::take();
        }
        if let (Some(account_id), Some(timings)) = (account_id, timings) {
            let message_count = fetched_count(&result);
            let _ = sync_kpi::write_last_sync_kpi(
                &self.path,
                &account_id,
                message_count,
                wall,
                &timings,
            );
        }
        result
    }

    /// Read back the last-run sync KPI snapshot for this profile as raw JSON
    /// (see `sync_kpi::read_last_sync_kpi`), or `Ok("null")` if no sync has
    /// completed in this profile yet — the FFI boundary always returns a
    /// string, so callers get a document to parse rather than a special-cased
    /// error status for "nothing recorded yet".
    pub fn last_sync_kpi_json(&self) -> Result<String, CoreError> {
        Ok(sync_kpi::read_last_sync_kpi(&self.path).unwrap_or_else(|| "null".to_string()))
    }

    /// A best-effort snapshot of the account's CURRENT (or most recently
    /// finished) sync pass — mailbox/message counts, never an error channel:
    /// `null` simply means no sync has run for this account yet this
    /// process. In-process only (see `sync_progress`), so this never touches
    /// `self.db` — every `Profile` sharing the process sees the same
    /// snapshot for a given `account_id`, which is fine since accounts are
    /// globally unique and only ever synced from one profile at a time.
    pub fn sync_progress_json(&self, account_id: &str) -> Result<String, CoreError> {
        sync_progress::snapshot_json(account_id)
    }

    /// Persist `config_json`'s account identity (id/email) without syncing —
    /// the "Save anyway" path for a connection check that failed. See
    /// `sync::save_account_without_sync_json` for why this exists and what it
    /// deliberately leaves untouched.
    pub fn save_account_without_sync_json(&self, config_json: &str) -> Result<(), CoreError> {
        sync::save_account_without_sync_json(&self.db, config_json)
    }

    /// Account-setup preflight (S62): connect + LOGIN + LIST over IMAP and
    /// connect + AUTH over SMTP, returning a `ConnectionTestReport` JSON with
    /// redacted per-leg results and the folder list. Blocking network I/O;
    /// deliberately never touches this profile's database, so a failed test
    /// persists nothing. A method on `Profile` only so the FFI can reuse the
    /// `profile_json` error/panic plumbing like every other command.
    pub fn test_connection_json(&self, config_json: &str) -> Result<String, CoreError> {
        connection_test::test_connection_json(config_json)
    }

    /// Hydrate one descriptor-only message body from IMAP and return the
    /// refreshed message detail JSON. `config_json` carries the account
    /// credentials; ordinary `get_message_json` remains local-only.
    pub fn hydrate_message_body_json(
        &self,
        config_json: &str,
        message_id: &str,
    ) -> Result<String, CoreError> {
        sync::hydrate_message_body_json(&self.db, config_json, message_id)
    }

    /// Run one bounded background body-hydration pass over the durable
    /// missing-body queue and return a JSON `HydrationReport`. An empty queue
    /// returns immediately without network I/O.
    pub fn hydrate_pending_bodies_json(&self, config_json: &str) -> Result<String, CoreError> {
        sync::hydrate_pending_bodies_json(&self.db, config_json)
    }

    /// Local-only: is this attachment already cached, and where. No network.
    pub fn attachment_cache_status_json(
        &self,
        message_id: &str,
        ordinal: i64,
    ) -> Result<String, CoreError> {
        attachments::attachment_cache_status_json(&self.db, &self.path, message_id, ordinal)
    }

    /// Download one attachment part over IMAP and cache it. Blocking network
    /// I/O unless already cached.
    pub fn download_attachment_json(
        &self,
        config_json: &str,
        message_id: &str,
        ordinal: i64,
    ) -> Result<String, CoreError> {
        attachments::download_attachment_json(
            &self.db,
            &self.path,
            config_json,
            message_id,
            ordinal,
        )
    }

    /// List a draft's attachment metadata (no bytes) as JSON.
    pub fn list_draft_attachments_json(&self, draft_id: i64) -> Result<String, CoreError> {
        let list = self
            .db
            .list_draft_attachments(draft_id)
            .map_err(|e| CoreError::Db(e.to_string()))?;
        serde_json::to_string(&list).map_err(|e| CoreError::Sync(format!("serialize: {e}")))
    }

    /// Add one attachment to a draft (bytes base64-decoded from
    /// `attachment_json`'s `data_base64`) and return the draft's refreshed
    /// attachment list as JSON.
    pub fn add_draft_attachment_json(
        &self,
        draft_id: i64,
        attachment_json: &str,
    ) -> Result<String, CoreError> {
        #[derive(serde::Deserialize)]
        struct NewDraftAttachment {
            filename: String,
            content_type: String,
            #[serde(default)]
            is_inline: bool,
            #[serde(default)]
            content_id: Option<String>,
            data_base64: String,
        }
        use base64::engine::general_purpose::STANDARD as BASE64;
        use base64::Engine;
        let a: NewDraftAttachment = serde_json::from_str(attachment_json)
            .map_err(|e| CoreError::Sync(format!("bad attachment json: {e}")))?;
        let bytes = BASE64
            .decode(&a.data_base64)
            .map_err(|e| CoreError::Sync(format!("bad base64: {e}")))?;
        self.db
            .add_draft_attachment(
                draft_id,
                &a.filename,
                &a.content_type,
                a.is_inline,
                a.content_id.as_deref(),
                &bytes,
            )
            .map_err(|e| CoreError::Db(e.to_string()))?;
        self.list_draft_attachments_json(draft_id)
    }

    /// Remove one attachment and return the draft's refreshed attachment list.
    pub fn remove_draft_attachment_json(
        &self,
        draft_id: i64,
        attachment_id: i64,
    ) -> Result<String, CoreError> {
        self.db
            .remove_draft_attachment(draft_id, attachment_id)
            .map_err(|e| CoreError::Db(e.to_string()))?;
        self.list_draft_attachments_json(draft_id)
    }

    /// Copy every attachment of `message_id` into `draft_id` (Forward),
    /// downloading any that are not already cached. Blocking network I/O.
    pub fn forward_attachments_json(
        &self,
        config_json: &str,
        message_id: &str,
        draft_id: i64,
    ) -> Result<String, CoreError> {
        attachments::copy_message_attachments_to_draft_json(
            &self.db,
            &self.path,
            config_json,
            message_id,
            draft_id,
        )
    }

    /// Local-only: is this message's raw RFC822 source already cached, and
    /// where. No network.
    pub fn raw_source_cache_status_json(&self, message_id: &str) -> Result<String, CoreError> {
        raw_source::raw_source_cache_status_json(&self.db, &self.path, message_id)
    }

    /// Fetch the message's full raw source over IMAP and cache it. Blocking
    /// network I/O unless already cached. Feeds both "Message Source" and
    /// `.eml` export (S49) — no persisted raw source exists otherwise.
    pub fn fetch_raw_source_json(
        &self,
        config_json: &str,
        message_id: &str,
    ) -> Result<String, CoreError> {
        raw_source::fetch_raw_source_json(&self.db, &self.path, config_json, message_id)
    }

    /// Read, parse and queue a local `.eml` file for import into
    /// `mailbox_id` on `account_id`, via the durable operation queue (same
    /// path `sent_copy` uses for APPEND). No network I/O — the import
    /// replays and syncs like any other queued mutation.
    pub fn import_eml_json(
        &self,
        account_id: &str,
        mailbox_id: &str,
        file_path: &str,
    ) -> Result<String, CoreError> {
        import_eml::import_eml_json(&self.db, account_id, mailbox_id, file_path)
    }

    /// Replay queued write-path operations (e.g. mark_read) to the IMAP server,
    /// returning a JSON report. Credentials are supplied in `config_json`.
    pub fn replay_operations_json(&self, config_json: &str) -> Result<String, CoreError> {
        replay::replay_imap_json(&self.db, config_json)
    }

    /// Generate the allowlisted, redacted support bundle as JSON. Strictly
    /// read-only: never writes to the database and never opens a connection.
    pub fn support_bundle_json(&self) -> Result<String, CoreError> {
        diagnostics::generate_support_bundle_json(&self.db)
    }

    /// Run the read-only SQLite integrity check (`PRAGMA quick_check`) and
    /// return `{"ok": bool, "issue_count": n, "first_issue": ...}` JSON.
    pub fn integrity_check_json(&self) -> Result<String, CoreError> {
        diagnostics::integrity_check_json(&self.db)
    }

    /// Generate the LOCAL-ONLY diagnostics view as JSON. Includes real account
    /// emails and mailbox paths (never the redacted ordinal labels). Error text
    /// is still the fixed redacted phrase — only identity is real.
    ///
    /// MUST NEVER be written to the exported support bundle or sent off-device.
    pub fn local_diagnostics_json(&self) -> Result<String, CoreError> {
        diagnostics::local_diagnostics_json(&self.db)
    }

    /// Deliver queued (`outbox`) drafts over SMTP, returning a JSON report.
    /// `config_json` carries the account, sender identity and SMTP endpoint.
    pub fn send_pending_json(&self, config_json: &str) -> Result<String, CoreError> {
        send::send_pending_json(&self.db, config_json)
    }

    /// Perform the RFC 8058 one-click unsubscribe POST for `message_id`.
    /// Never called except after the Swift confirmation sheet has already
    /// shown the exact destination and the user tapped "Unsubscribe".
    pub fn unsubscribe_one_click_json(&self, message_id: &str) -> Result<String, CoreError> {
        unsubscribe::unsubscribe_one_click_json(&self.db, message_id)
    }

    // --- AI job queue --------------------------------------------------------

    /// Enqueue an AI job (summarise/draft) and return `{"job_id": n}`.
    /// `request_json` must include `feature` + refs (`message_id` / `thread_id`).
    pub fn enqueue_ai_job_json(&self, request_json: &str) -> Result<String, CoreError> {
        ai::enqueue_ai_job_json(&self.db, request_json)
    }

    /// Run pending AI jobs via a local Ollama instance, returning a JSON report.
    /// `config_json` must include `endpoint`, `model`, and optional tuning.
    pub fn run_ai_jobs_json(&self, config_json: &str) -> Result<String, CoreError> {
        ai::run_ai_jobs_json(&self.db, config_json)
    }

    /// Long-poll a job for its current state + new events after `after_event_id`.
    pub fn poll_ai_job_json(&self, job_id: i64, after_event_id: i64) -> Result<String, CoreError> {
        ai::poll_ai_job_json(&self.db, job_id, after_event_id)
    }

    /// Request cooperative cancellation of an in-flight AI job.
    pub fn cancel_ai_job(&self, job_id: i64) -> Result<(), CoreError> {
        ai::cancel_ai_job(&self.db, job_id)
    }

    /// List models available at the Ollama endpoint. `config_json` must include `endpoint`.
    pub fn ai_list_models_json(&self, config_json: &str) -> Result<String, CoreError> {
        ai::ai_list_models_json(config_json)
    }

    /// Retrieve a cached AI output (summary or draft) by its natural key.
    /// Returns `None` if no output has been persisted for this scope/owner/feature.
    pub fn get_ai_output_json(
        &self,
        scope: &str,
        owner_id: &str,
        feature: &str,
    ) -> Result<Option<String>, CoreError> {
        ai::get_ai_output_json(&self.db, scope, owner_id, feature)
    }

    /// Retrieve a thread's typed extraction rows (`extract_thread_items`,
    /// S54) as a JSON array — empty, never an error, for a thread with
    /// nothing extracted yet.
    pub fn get_thread_extractions_json(&self, thread_id: &str) -> Result<String, CoreError> {
        ai::get_thread_extractions_json(&self.db, thread_id)
    }

    /// "Find similar" (S53-UI): embed `message_id` (ad-hoc, reusing its
    /// stored embedding when one exists), then find its nearest neighbours
    /// by cosine similarity, as `{ results: [{ header, similarity }] }`.
    /// `config_json` is the same shape `run_ai_jobs_json` takes (endpoint,
    /// model, optional embedding_model). `account_id: None` searches every
    /// account. May take longer than other search calls (a network round
    /// trip to Ollama on the ad-hoc-embed path) — the FFI caller must run
    /// this off the Swift main thread.
    pub fn find_similar_json(
        &self,
        config_json: &str,
        message_id: &str,
        account_id: Option<&str>,
        limit: i64,
    ) -> Result<String, CoreError> {
        let config: ai::run::AiRunConfig = serde_json::from_str(config_json)
            .map_err(|e| CoreError::Sync(format!("bad ai run config json: {e}")))?;
        ai::find_similar_json(&self.db, &config, message_id, account_id, limit)
    }

    // --- IMAP IDLE supervision (S31) ------------------------------------

    /// Start (or restart) `config_json`'s account's supervised IDLE thread.
    /// `config_json` is the same `ImapSyncConfig` shape `sync_imap_json`
    /// takes. Needs `&Arc<Self>` (not plain `&self`) because the spawned
    /// thread must outlive this one call — it holds its own clone of the
    /// profile for as long as it runs.
    pub fn start_idle(self: &Arc<Self>, config_json: &str) -> Result<(), CoreError> {
        let cfg: sync::ImapSyncConfig = serde_json::from_str(config_json)
            .map_err(|e| CoreError::Sync(format!("bad idle config json: {e}")))?;
        self.idle.start(Arc::clone(self), cfg);
        Ok(())
    }

    /// Stop `account_id`'s IDLE thread if one is running. Blocks until it has
    /// fully exited.
    pub fn stop_idle(&self, account_id: &str) {
        self.idle.stop(account_id);
    }

    /// Wake `account_id`'s IDLE thread now (sleep/wake, network change). A
    /// no-op if that account has no running thread.
    pub fn nudge_idle(&self, account_id: &str) {
        self.idle.nudge(account_id);
    }

    /// Whether `account_id` currently has a PROVEN, actively-idling IDLE
    /// connection — false while connecting, backed off after a failure, or
    /// once the server is confirmed not to advertise IDLE, never merely
    /// "a background thread object still exists". Purely in-memory — never
    /// persisted, never in a support bundle.
    pub fn is_idle_running(&self, account_id: &str) -> bool {
        self.idle.is_running(account_id)
    }

    /// Count of IDLE-thread resync passes finished so far in this process.
    /// A pass can rewrite message rows (hydrated previews and bodies, sort
    /// keys) with NO new arrival, so the new-mail outbox stays empty; the app
    /// polls this cheap counter and reloads the open list when it moves
    /// (#3040). Purely in-memory, starts at 0 for every opened profile.
    pub fn idle_pass_epoch(&self) -> u64 {
        self.idle_pass_epoch
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Stop every account's IDLE thread, blocking until all have exited.
    /// Called from `castellum_close_profile` before the profile is dropped,
    /// so closing never leaves a background connection (and the `Arc` it
    /// holds) alive past the close call.
    pub fn stop_all_idle(&self) {
        self.idle.stop_all();
    }
}

/// Best-effort extraction of `account_id` from a sync `config_json` blob,
/// without deserializing the full `ImapSyncConfig` (which is private to
/// `sync` and additionally requires the password field). Used only to label
/// a KPI snapshot; `None` (malformed JSON, missing field) just means no
/// snapshot is written for that call — the sync itself is unaffected either
/// way, since `sync::sync_imap_json` does its own, authoritative parse.
fn account_id_from_config(config_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(config_json).ok()?;
    v.get("account_id")?.as_str().map(str::to_string)
}

/// `orphaned_attachment_hashes` of a sync report (see `SyncReport`); empty
/// when the sync failed or removed no folder.
fn orphaned_attachment_hashes(result: &Result<String, CoreError>) -> Vec<String> {
    let Ok(json) = result else { return Vec::new() };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    v["orphaned_attachment_hashes"]
        .as_array()
        .map(|hashes| {
            hashes
                .iter()
                .filter_map(|h| h.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Sum of every mailbox's `fetched` count from a `sync::sync_imap_json`
/// result, or 0 when the sync failed or the report shape is unexpected. Only
/// ever reads a count field, never message content.
fn fetched_count(result: &Result<String, CoreError>) -> i64 {
    let Ok(json) = result else { return 0 };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return 0;
    };
    v["mailboxes"]
        .as_array()
        .map(|mailboxes| {
            mailboxes
                .iter()
                .filter_map(|mb| mb["fetched"].as_i64())
                .sum()
        })
        .unwrap_or(0)
}

/// Open (creating if necessary) the profile rooted at `path`: ensure the
/// directory exists, open `<path>/castellum.db`, and apply pending migrations.
///
/// The path is always explicit — the core never defaults to any location.
pub fn open_profile(path: &Path) -> Result<Profile, CoreError> {
    if path.as_os_str().is_empty() {
        return Err(CoreError::InvalidArgument);
    }
    std::fs::create_dir_all(path).map_err(|_| CoreError::Io)?;
    let db = castellum_db::Database::open(&path.join(DB_FILE))?;
    Ok(Profile {
        path: path.to_path_buf(),
        db,
        idle: idle::IdleSupervisor::new(),
        idle_pass_epoch: std::sync::atomic::AtomicU64::new(0),
    })
}

/// Close a profile, releasing its resources (closes the SQLite connection).
pub fn close_profile(profile: Profile) {
    drop(profile);
}

/// Cheaply report how many migrations are pending for the profile rooted at
/// `path`, WITHOUT opening it for real and without running any of them —
/// see `castellum_db::Database::pending_migration_count`'s doc comment.
/// Used by the Swift launch preflight to decide whether to
/// show the "Upgrading your mail database…" window before calling
/// [`open_profile`]. `Err(CoreError::SchemaTooNew(_))` reuses the same
/// classification `open_profile` would produce, so the existing #169
/// refusal path still wins for a profile this build can't safely open at
/// all.
pub fn pending_migrations(path: &Path) -> Result<castellum_db::PendingMigrations, CoreError> {
    if path.as_os_str().is_empty() {
        return Err(CoreError::InvalidArgument);
    }
    Ok(castellum_db::Database::pending_migration_count(
        &path.join(DB_FILE),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_tmp() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        // Unique per RUN: see the note in castellum_db::ai::tests::fresh_db.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        castellum_testutil::scratch_root().join(format!(
            "castellum-core-test-{}-{stamp}-{n}",
            std::process::id()
        ))
    }

    #[test]
    fn version_is_non_empty() {
        assert!(!version().is_empty());
    }

    #[test]
    fn open_creates_db_and_migrates() {
        let dir = unique_tmp();
        assert!(!dir.exists());
        let profile = open_profile(&dir).expect("open should succeed");
        assert!(dir.join(DB_FILE).exists(), "database file created");
        assert_eq!(
            profile.schema_version().unwrap(),
            castellum_db::latest_schema_version()
        );
        close_profile(profile);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_is_idempotent() {
        let dir = unique_tmp();
        let v1 = open_profile(&dir)
            .expect("first open")
            .schema_version()
            .unwrap();
        let p2 = open_profile(&dir).expect("second open on existing profile");
        assert_eq!(p2.schema_version().unwrap(), v1);
        close_profile(p2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `pending_migrations` mirrors `open_profile`'s classification without
    /// ever opening the profile for real ('s launch preflight
    /// probe).
    #[test]
    fn pending_migrations_reports_all_pending_then_zero_after_a_real_open() {
        let dir = unique_tmp();
        assert!(!dir.exists());
        let before = pending_migrations(&dir).expect("probe on a nonexistent profile");
        assert!(before.pending_count > 0);
        assert!(
            !dir.exists(),
            "the probe must never create the profile directory"
        );

        let profile = open_profile(&dir).expect("open");
        close_profile(profile);

        let after = pending_migrations(&dir).expect("probe on an up-to-date profile");
        assert_eq!(after.pending_count, 0);
        assert!(!after.has_heavy);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_migrations_rejects_an_empty_path() {
        match pending_migrations(Path::new("")) {
            Err(CoreError::InvalidArgument) => {}
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[test]
    fn draft_attachment_json_round_trips_base64_and_rejects_bad_input() {
        let dir = unique_tmp();
        let profile = open_profile(&dir).expect("open");
        profile
            .db
            .upsert_account(&castellum_models::Account {
                id: "acc".into(),
                email: "a@example.com".into(),
                display_name: None,
                provider: "imap".into(),
            })
            .unwrap();
        let draft_id = profile
            .save_draft_json(
                r#"{"account_id":"acc","to":[{"email":"b@example.com"}],"subject":"s","body_text":"b"}"#,
            )
            .unwrap();
        let out = profile
            .add_draft_attachment_json(
                draft_id,
                r#"{"filename":"a.txt","content_type":"text/plain","data_base64":"aGVsbG8="}"#,
            )
            .unwrap();
        let list: Vec<castellum_models::DraftAttachmentMeta> = serde_json::from_str(&out).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].size_bytes, 5);
        assert_eq!(
            profile
                .db
                .draft_attachment_bytes(list[0].id)
                .unwrap()
                .unwrap(),
            b"hello"
        );
        assert!(profile
            .add_draft_attachment_json(
                draft_id,
                r#"{"filename":"a","content_type":"text/plain","data_base64":"!!!"}"#
            )
            .is_err());
        let out = profile
            .remove_draft_attachment_json(draft_id, list[0].id)
            .unwrap();
        assert_eq!(out, "[]");
        close_profile(profile);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_path_is_invalid_argument() {
        match open_profile(Path::new("")) {
            Err(e) => assert_eq!(e.status(), CastellumStatus::InvalidArgument),
            Ok(_) => panic!("empty path should be rejected"),
        }
    }
}
