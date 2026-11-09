// Castellum - SQLite persistence spine.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Slice 2 scope: open a real profile database, apply forward migrations
// idempotently, and report the schema version. One connection guarded by a
// Mutex — writes are serialized (the "one-writer" model from the decision log).
// The async writer actor (Tokio) is deferred until the first async work (IMAP
// sync) needs it. The full domain schema (accounts/messages/threads/...) lands
// with that sync work; slice 2 only proves the spine end to end.

use rusqlite::Connection;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

mod ai;
mod comm_health;
mod contacts;
mod diagnostics;
mod enrichment;
mod envelope_repair;
mod filter_sql;
mod identities;
mod inbox_splits;
mod links;
mod list_page;
mod message_headers;
pub mod notify;
mod project_timeline;
mod projects;
mod provenance;
mod repo;
mod rules;
mod saved_views;
mod search_query;
mod smart_mailboxes;
mod sort_fold;
mod suggestions;
pub mod tags;
mod thread_extractions;
mod unassigned_mail;
mod work_items;

pub use diagnostics::{
    AccountSyncState, IntegrityCheckResult, MailboxBodyCounts, MailboxSyncState,
};

pub use ai::{
    AiEventRecord, AiJobRecord, AiOutputRecord, MessageEmbeddingRecord, AI_PRIORITY_BACKGROUND,
};
pub use comm_health::{
    compute_signals, CommunicationHealth, HealthSignal, MailEvent, WaitingItem, EVIDENCE_CAP,
};
pub use enrichment::EnrichmentRow;
pub use envelope_repair::EnvelopeTextRepairBatch;
pub use inbox_splits::{OTHER_SPLIT_ID, OTHER_SPLIT_NAME};
pub use links::{EntityKind, LinkEnd, LinkRecord, LinkUpsert};
pub use list_page::{ListCursor, ListSort, MessagePage, SortKey};
pub use project_timeline::{TimelineEntry, TimelineKind};
pub use projects::ProjectUpsert;
pub use provenance::WorkItemProvenance;
pub use repo::{
    FullThreadRebuildReason, MoveBatchOutcome, MoveRefusal, MoveTarget, RemovedMailboxes,
    ThreadRebuildPath, UndoMoveOutcome, UndoRefusal, EVALUATION_MODE_MSG, LATE_SEND_GRACE_SECONDS,
};
pub use rules::{RulePreview, SavedFilterUpsert, RULE_ID_CHUNK, RULE_PROTECTED_ROLES};
pub use saved_views::{parse_view_filter, SavedView, SavedViewCount, BUILTIN_VIEW_PREFIX};
pub use smart_mailboxes::SmartMailboxUpsert;
pub use suggestions::{
    MessageContext, NewSuggestion, Suggestion, SuggestionAccepted, SuggestionFilter,
    SUGGESTION_KINDS, SUGGESTION_ORIGINS, SUGGESTION_STATUSES,
};
pub use thread_extractions::{NewThreadExtraction, ThreadExtractionRecord};
pub use unassigned_mail::{UnassignedMailPage, UnassignedThread};
pub use work_items::{
    local_utc_offset_secs, DueWindow, WorkItem, WorkItemFilter, WorkItemUpsert,
    WORK_ITEM_LINK_RELATION,
};

/// A database error. Carries a human-readable message for logs/tests; the FFI
/// layer maps it to a stable status code.
#[derive(Debug)]
pub struct DbError(pub String);

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for DbError {}

impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        DbError(e.to_string())
    }
}

/// A single forward-only schema migration. Ids are strictly ascending (gaps
/// allowed) and applied in order; each runs once, recorded in `_migrations`.
struct Migration {
    id: i64,
    name: &'static str,
    sql: &'static str,
}

/// Migration ids whose SQL rebuilds a large table (and its index/virtual
/// table) from scratch, so it needs the OLD and NEW copy to coexist on disk
/// at once mid-migration — far more headroom than a plain `ALTER TABLE` or a
/// `CREATE TABLE`/`CREATE INDEX` alongside existing data. Checked by
/// `run_migration_set`'s free-space precheck (PR #178 review hardening,
/// following #169's backup-only precheck) before it takes any backup or
/// touches any data. 0038 ('s `message_fts` rowid rebuild) is
/// the first; add a new id here whenever a future migration does the same
/// (drops+recreates a big table/index rather than an additive change).
const HEAVY_MIGRATION_IDS: &[i64] = &[38];

fn is_heavy_migration(id: i64) -> bool {
    HEAVY_MIGRATION_IDS.contains(&id)
}

/// The ordered migration set. Append new migrations with the next id; never edit
/// or reorder an already-shipped migration.
const MIGRATIONS: &[Migration] = &[
    Migration {
        id: 1,
        name: "init_app_meta",
        sql: "CREATE TABLE app_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    },
    Migration {
        id: 2,
        name: "core_mail",
        sql: include_str!("migrations/0002_core_mail.sql"),
    },
    Migration {
        id: 3,
        name: "fts",
        sql: include_str!("migrations/0003_fts.sql"),
    },
    Migration {
        id: 4,
        name: "threads",
        sql: include_str!("migrations/0004_threads.sql"),
    },
    Migration {
        id: 5,
        name: "operations",
        sql: include_str!("migrations/0005_operations.sql"),
    },
    Migration {
        id: 6,
        name: "html_bodies",
        sql: include_str!("migrations/0006_html_bodies.sql"),
    },
    Migration {
        id: 7,
        name: "drafts",
        sql: include_str!("migrations/0007_drafts.sql"),
    },
    // id 8 intentionally unused (was reserved during parallel development;
    // ids need not be contiguous — the watermark is MAX(id), so gaps apply
    // cleanly, but ids may never be inserted below the current max).
    Migration {
        id: 9,
        name: "contacts",
        sql: include_str!("migrations/0009_contacts.sql"),
    },
    Migration {
        id: 10,
        name: "ai",
        sql: include_str!("migrations/0010_ai.sql"),
    },
    Migration {
        id: 11,
        name: "projects",
        sql: include_str!("migrations/0011_projects.sql"),
    },
    Migration {
        id: 12,
        name: "enrichment",
        sql: include_str!("migrations/0012_enrichment.sql"),
    },
    Migration {
        id: 13,
        name: "mailbox_discovery",
        sql: include_str!("migrations/0013_mailbox_discovery.sql"),
    },
    Migration {
        id: 14,
        name: "attachment_descriptors",
        sql: include_str!("migrations/0014_attachment_descriptors.sql"),
    },
    Migration {
        id: 15,
        name: "operation_blobs",
        sql: include_str!("migrations/0015_operation_blobs.sql"),
    },
    Migration {
        id: 16,
        name: "sync_diagnostics",
        sql: include_str!("migrations/0016_sync_diagnostics.sql"),
    },
    Migration {
        id: 17,
        name: "links",
        sql: include_str!("migrations/0017_links.sql"),
    },
    Migration {
        id: 18,
        name: "work_items",
        sql: include_str!("migrations/0018_work_items.sql"),
    },
    Migration {
        id: 19,
        name: "account_write_policy",
        sql: include_str!("migrations/0019_account_write_policy.sql"),
    },
    Migration {
        id: 20,
        name: "undo_send",
        sql: include_str!("migrations/0020_undo_send.sql"),
    },
    Migration {
        id: 21,
        name: "attachment_addressing",
        sql: include_str!("migrations/0021_attachment_addressing.sql"),
    },
    Migration {
        id: 22,
        name: "attachment_cache",
        sql: include_str!("migrations/0022_attachment_cache.sql"),
    },
    Migration {
        id: 23,
        name: "unified_inbox_indexes",
        sql: include_str!("migrations/0023_unified_inbox_indexes.sql"),
    },
    Migration {
        id: 24,
        name: "snoozed_messages",
        sql: include_str!("migrations/0024_snoozed_messages.sql"),
    },
    Migration {
        id: 25,
        name: "markdown_compose",
        sql: include_str!("migrations/0025_markdown_compose.sql"),
    },
    Migration {
        id: 26,
        name: "account_identities",
        sql: include_str!("migrations/0026_account_identities.sql"),
    },
    Migration {
        id: 27,
        name: "list_unsubscribe",
        sql: include_str!("migrations/0027_list_unsubscribe.sql"),
    },
    Migration {
        id: 28,
        name: "smart_mailboxes",
        sql: include_str!("migrations/0028_smart_mailboxes.sql"),
    },
    Migration {
        id: 29,
        name: "trust_surface",
        sql: include_str!("migrations/0029_trust_surface.sql"),
    },
    Migration {
        id: 30,
        name: "draft_attachments",
        sql: include_str!("migrations/0030_draft_attachments.sql"),
    },
    Migration {
        id: 31,
        name: "message_embeddings",
        sql: include_str!("migrations/0031_message_embeddings.sql"),
    },
    Migration {
        id: 32,
        name: "calendar_invites",
        sql: include_str!("migrations/0032_calendar_invites.sql"),
    },
    Migration {
        id: 33,
        name: "thread_extractions",
        sql: include_str!("migrations/0033_thread_extractions.sql"),
    },
    Migration {
        id: 34,
        name: "saved_filters",
        sql: include_str!("migrations/0034_saved_filters.sql"),
    },
    Migration {
        id: 35,
        name: "server_drafts",
        sql: include_str!("migrations/0035_server_drafts.sql"),
    },
    Migration {
        id: 36,
        name: "contact_lookup_index",
        sql: include_str!("migrations/0036_contact_lookup_index.sql"),
    },
    Migration {
        id: 37,
        name: "thread_rebuild_dirty",
        sql: include_str!("migrations/0037_thread_rebuild_dirty.sql"),
    },
    Migration {
        id: 38,
        name: "fts_rowid_index",
        sql: include_str!("migrations/0038_fts_rowid_index.sql"),
    },
    Migration {
        id: 39,
        name: "thread_dirty_messages",
        sql: include_str!("migrations/0039_thread_dirty_messages.sql"),
    },
    // 39 (PR #181, incremental threads) merged to main first, then this one
    // (PR #180, chunked descriptor fetch) took 40, exactly as both PRs
    // planned — no renumbering was needed.
    Migration {
        id: 40,
        name: "descriptor_chunk_checkpoint",
        sql: include_str!("migrations/0040_descriptor_chunk_checkpoint.sql"),
    },
    // 41: S42 tags and mute (PR #173). Written as 0039 on its branch, which
    // main's #181 then claimed; renumbered at merge time so a profile already
    // at the 39/40 watermark still applies it.
    Migration {
        id: 41,
        name: "tags_and_mute",
        sql: include_str!("migrations/0041_tags_and_mute.sql"),
    },
    // 42: MM-1 smart-mailbox nested groups (PR #201). Planned as 42 with 41
    // left for #173; #173 merged first and took 41, so no renumbering.
    Migration {
        id: 42,
        name: "smart_mailbox_groups",
        sql: include_str!("migrations/0042_smart_mailbox_groups.sql"),
    },
    // 43: MM-3 rule order, stop-processing and enabled switch (PR #203).
    Migration {
        id: 43,
        name: "rule_order",
        sql: include_str!("migrations/0043_rule_order.sql"),
    },
    // 44: MM-6 identity Reply-To, auto-Bcc and default flag (PR #206).
    Migration {
        id: 44,
        name: "identity_details",
        sql: include_str!("migrations/0044_identity_details.sql"),
    },
    // 45: AW1 work-item due/waiting/completed dates, delegate, nullable project.
    Migration {
        id: 45,
        name: "work_item_dates",
        sql: include_str!("migrations/0045_work_item_dates.sql"),
    },
    // 46: AW2 suggestions you accept (proposals, evidence as ids only).
    Migration {
        id: 46,
        name: "suggestions",
        sql: include_str!("migrations/0046_suggestions.sql"),
    },
    // 47: AW3 saved Active Work views (built-ins seeded by Rust on first use).
    Migration {
        id: 47,
        name: "saved_views",
        sql: include_str!("migrations/0047_saved_views.sql"),
    },
    // 48: S59 VIP senders + notification outbox.
    Migration {
        id: 48,
        name: "notifications_vip",
        sql: include_str!("migrations/0048_notifications_vip.sql"),
    },
    // 49: Active Work append-only project activity + decision timeline.
    Migration {
        id: 49,
        name: "project_timeline",
        sql: include_str!("migrations/0049_project_timeline.sql"),
    },
    // 50: work-item provenance (message/suggestion/manual source records).
    Migration {
        id: 50,
        name: "work_item_provenance",
        sql: include_str!("migrations/0050_work_item_provenance.sql"),
    },
    // 51: (account_id, rfc822_message_id) index for the rules dedupe window.
    Migration {
        id: 51,
        name: "messages_account_rfc822_index",
        sql: include_str!("migrations/0051_messages_account_rfc822_index.sql"),
    },
    Migration {
        id: 52,
        name: "ai_jobs_embed_message_index",
        sql: include_str!("migrations/0052_ai_jobs_embed_message_index.sql"),
    },
    // 53: calendar invite LOCATION / DESCRIPTION (#3041).
    Migration {
        id: 53,
        name: "calendar_invite_location",
        sql: include_str!("migrations/0053_calendar_invite_location.sql"),
    },
    // 54: calendar invite RECURRENCE-ID (#3446).
    Migration {
        id: 54,
        name: "calendar_invite_recurrence_id",
        sql: include_str!("migrations/0054_calendar_invite_recurrence_id.sql"),
    },
    // 55: UIDs to re-fetch after a cancelled archive/delete/Empty Trash (bug hunt #69).
    Migration {
        id: 55,
        name: "pending_refetch",
        sql: include_str!("migrations/0055_pending_refetch.sql"),
    },
    // 56: arrivals parked until their body exists, for Body/Attachment rules (bug hunt #50).
    Migration {
        id: 56,
        name: "pending_rules",
        sql: include_str!("migrations/0056_pending_rules.sql"),
    },
    // 57: stored message headers for any-header smart-mailbox conditions (MM-1c, #2710).
    Migration {
        id: 57,
        name: "message_headers",
        sql: include_str!("migrations/0057_message_headers.sql"),
    },
];

/// Seconds a decode-skip retry-set row waits before its next retry, given
/// how many retries have already FAILED (`attempts`): a fresh skip (0) waits
/// 1 hour, then 4 hours, then 24 hours, then 7 days for every retry after
/// that (the cap). Review of PR #180, finding 3: without a backoff a
/// permanently undecodable message cost reconnects on every sync, forever.
pub fn decode_skip_retry_delay_secs(attempts: i64) -> i64 {
    const HOUR: i64 = 3600;
    match attempts {
        i64::MIN..=0 => HOUR,
        1 => 4 * HOUR,
        2 => 24 * HOUR,
        _ => 7 * 24 * HOUR,
    }
}

/// The highest migration id known to this build.
pub fn latest_schema_version() -> i64 {
    MIGRATIONS.iter().map(|m| m.id).max().unwrap_or(0)
}

/// Stable prefix on [`OpenError::SchemaTooNew`]'s `Display` text, matched by
/// `CastellumLaunchError` on the Swift side the same way
/// `AccountWritePolicy.evaluationModePrefix` matches `EVALUATION_MODE_MSG` —
/// a plain status code can't carry which of the two launch-refusal cases
/// this is, so the message itself is the typed channel. Pinned by
/// `open_error_prefixes_stay_stable_for_the_swift_classifier` below; never
/// reword without updating the Swift classifier in the same change.
pub const SCHEMA_TOO_NEW_PREFIX: &str = "schema too new: ";
/// Stable prefix on [`OpenError::MigrationFailed`]'s `Display` text. See
/// [`SCHEMA_TOO_NEW_PREFIX`].
pub const MIGRATION_FAILED_PREFIX: &str = "migration failed: ";

/// Why [`Database::open`] refused to open (or finish upgrading) a profile.
/// Distinct from [`DbError`] (used by every other method in this crate)
/// because opening alone must distinguish two launch-blocking cases from a
/// generic I/O failure:
///
/// - an OLDER build opening a profile a NEWER build already migrated further
///   (e.g. after a user downgrades) — refused before any write, via
///   [`OpenError::SchemaTooNew`];
/// - a migration that fails partway — its own transaction rolls back
///   cleanly (see `run_migration_set`), so the watermark never advances, and
///   a pre-migration backup is taken first for a non-empty profile — via
///   [`OpenError::MigrationFailed`].
///
/// `Display` composes the full, non-technical, user-facing sentence for
/// each — the C ABI rule is "Rust owns strings", so the Swift side only
/// strips the machine-matched prefix and shows the rest verbatim rather than
/// re-composing the message itself.
#[derive(Debug)]
pub enum OpenError {
    /// A filesystem/SQLite error unrelated to schema version or migration
    /// content (bad path, permissions, a corrupt file, ...).
    Io(String),
    /// The profile's applied schema (`found`) is newer than the highest
    /// migration this build knows (`supported`).
    SchemaTooNew { found: i64, supported: i64 },
    /// Migration `id` (`name`) failed to apply; `reason` is the underlying
    /// SQLite error. `backup_path` is the pre-migration backup's location,
    /// present whenever the profile was non-empty (a brand-new profile never
    /// gets one — nothing to protect yet).
    MigrationFailed {
        id: i64,
        name: String,
        reason: String,
        backup_path: Option<String>,
    },
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Io(s) => write!(f, "{s}"),
            OpenError::SchemaTooNew { found, supported } => write!(
                f,
                "{SCHEMA_TOO_NEW_PREFIX}This profile was created by a newer version of \
                 Castellum (schema {found}). Install the latest version of Castellum to open \
                 it. This build supports up to schema {supported}."
            ),
            // Deliberately non-technical: `id`/`name`/`reason` are the
            // exact SQLite failure and are logged (`tracing::error!` in
            // `run_migration_set`) for diagnostics, not shown here — this
            // crosses the FFI boundary verbatim to become the launch alert
            // text (`CastellumLaunchError`), so it stays a sentence a user
            // can read.
            OpenError::MigrationFailed { backup_path, .. } => {
                write!(
                    f,
                    "{MIGRATION_FAILED_PREFIX}Castellum couldn't upgrade your mail database. \
                     Your data is unchanged."
                )?;
                if let Some(p) = backup_path {
                    write!(f, " A backup was saved at {p}.")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for OpenError {}

/// What [`Database::pending_migration_count`]'s cheap, read-only probe
/// found. See that function's doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingMigrations {
    /// The applied schema watermark THIS PROBE observed (`MAX(id)` in
    /// `_migrations`, or 0 for a profile that doesn't exist yet / has never
    /// been migrated). Callers use this to distinguish a genuinely EXISTING
    /// profile with pending work (show the migrating window) from a
    /// brand-new one, where `pending_count` is also nonzero (every
    /// migration this build knows) but there is nothing to protect and
    /// `Database::open` creating+migrating it from empty is fast enough to
    /// need no window.
    pub applied_version: i64,
    /// How many migrations this build knows about that the profile hasn't
    /// applied yet. 0 for an up-to-date profile.
    pub pending_count: i64,
    /// Whether any of those pending migrations is HEAVY (see
    /// `is_heavy_migration`'s doc comment) — informational only; the Swift
    /// side uses it to word the window, never to change whether the
    /// migration itself runs.
    pub has_heavy: bool,
}

impl From<rusqlite::Error> for OpenError {
    fn from(e: rusqlite::Error) -> Self {
        OpenError::Io(e.to_string())
    }
}

/// An open profile database. Cloneable handle is intentionally NOT provided —
/// ownership lives in the core `Profile`.
pub struct Database {
    conn: Mutex<Connection>,
    /// S25 undo-send: account ids whose crash-safety hold
    /// (`repo::Database::reconcile_undo_sends`) has already run against THIS
    /// open `Database` (i.e. this launch). Guards the reconcile itself to run
    /// at most once per account per launch — both so a legitimate in-session
    /// undo window is never re-held, and so `castellum_core::send::
    /// send_pending_with_sender` can safely call through the same gate before
    /// every drain without repeating the hold pass. See DECISIONS.md.
    reconciled_undo_send_accounts: Mutex<HashSet<String>>,
    /// Operation ids this open `Database` (this launch) has claimed with
    /// `begin_operation_attempt[_from]`. `running` alone
    /// cannot tell "in flight right now" from "orphaned by a crash", so a
    /// `running` op is re-claimable only when it is NOT in this set, i.e.
    /// an earlier launch claimed it and died. Only ever locked while `conn`
    /// is held (the claim checks and flips the row under both), never the
    /// other way round, so it cannot deadlock. An entry is dropped when the
    /// op leaves `running` through `set_operation_status`; a stale entry is
    /// harmless because the guard only applies while the row is `running`.
    claimed_operations: Mutex<HashSet<i64>>,
    /// Serialises thread-projection rebuilds (PR #181 review, finding 2):
    /// held for the whole of `rebuild_threads` and
    /// `rebuild_threads_incremental`, so two rebuilds of the same profile
    /// never interleave -- the app's `performSync` (Swift `AppModel`) has no
    /// per-account in-flight guard, so a retry, a `reSyncAll` and a
    /// replay-triggered sync can all reach a rebuild at once. Writers never
    /// take it (they only need `conn`), and a rebuild always takes it BEFORE
    /// `conn`, never while holding it, so the two locks cannot deadlock.
    thread_rebuild_lock: Mutex<()>,
    /// Serialises contacts-aggregate refreshes (S72). A refresh no longer
    /// runs inside one transaction under `conn` (it reads a snapshot on its
    /// own connection and writes in short chunks), so two concurrent ones
    /// could interleave their chunk writes. Same ordering rule as
    /// `thread_rebuild_lock`: always taken BEFORE `conn`, never while
    /// holding it.
    contacts_refresh_lock: Mutex<()>,
    /// The database file, for opening short-lived read-only snapshot
    /// connections (WAL readers never wait on `conn`). `None` only for the
    /// migration tests' hand-built instances.
    path: Option<std::path::PathBuf>,
}

/// The `-journal` sidecar SQLite writes next to `path` while it's mid-write
/// (here: a `VACUUM INTO` target). Shared by the write-attempt and the
/// cleanup-on-failure paths in `Database::take_pre_migration_backup` so they
/// can't drift apart on the exact suffix.
fn journal_sidecar(path: &Path) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push("-journal");
    std::path::PathBuf::from(s)
}

/// Remove a backup artifact that may be a plain file OR (in one test's
/// simulated failure — see `a_backup_target_blocked_by_a_directory_...`) a
/// directory occupying the same name. Best-effort either way: a failure to
/// clean up a leftover must never block opening the profile.
fn remove_backup_artifact(path: &Path) {
    if std::fs::remove_file(path).is_err() {
        let _ = std::fs::remove_dir_all(path);
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only seam for `free_space_bytes`: when set, short-circuits the
    /// real `statvfs(2)` call so a test can force the "not enough room to
    /// back up safely" path without actually filling a disk. `thread_local`
    /// (not a plain static) because `cargo test` runs each test on its own
    /// thread, which keeps tests that don't touch this isolated from ones
    /// that do.
    static FREE_SPACE_OVERRIDE_FOR_TEST: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Bytes available to the current user on the filesystem containing `dir`,
/// via `statvfs(2)` (macOS, matching the rest of this app — see the `libc`
/// dependency comment in `Cargo.toml`). `None` if the check itself fails
/// (bad path, an exotic filesystem, ...) — callers treat that as "can't
/// tell, proceed" rather than blocking a backup that might otherwise
/// succeed; the precheck is a best-effort guard, not a correctness
/// requirement (a subsequent `VACUUM INTO` failure is still caught and
/// cleaned up on its own).
fn free_space_bytes(dir: &Path) -> Option<u64> {
    #[cfg(test)]
    if let Some(v) = FREE_SPACE_OVERRIDE_FOR_TEST.with(|c| c.get()) {
        return Some(v);
    }
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c_path = CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: `stat` is a plain-old-data struct zero-initialised before the
    // call, and `statvfs` only ever writes into it (or leaves it untouched
    // and returns nonzero) — never reads it first.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    Some((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

impl Database {
    /// Open (creating if needed) the SQLite database at `path`, set durable
    /// pragmas, and run any pending migrations. Idempotent across reopens.
    ///
    /// Refuses (before any write beyond the idempotent `_migrations` table
    /// creation) a profile whose applied schema is newer than this build
    /// supports — see [`OpenError::SchemaTooNew`] — and rolls back cleanly,
    /// backup in hand, on a failing migration — see
    /// [`OpenError::MigrationFailed`].
    pub fn open(path: &Path) -> Result<Database, OpenError> {
        let conn = Connection::open(path)?;
        search_query::register_sql_functions(&conn)?;
        // WAL for concurrent readers + a single writer; enforce FKs; wait rather
        // than fail on transient lock contention.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        conn.busy_timeout(Duration::from_secs(5))?;
        // rusqlite's default LRU capacity (16) is smaller than the distinct
        // literal statements `upsert_message`/`upsert_messages_batch` alone
        // reuse via `prepare_cached` -- undersized, every
        // batched upsert would evict and re-prepare mid-message. 64 covers
        // that hot path with headroom for the rest of this connection's
        // long lifetime.
        conn.set_prepared_statement_cache_capacity(64);
        let db = Database {
            conn: Mutex::new(conn),
            reconciled_undo_send_accounts: Mutex::new(HashSet::new()),
            claimed_operations: Mutex::new(HashSet::new()),
            thread_rebuild_lock: Mutex::new(()),
            contacts_refresh_lock: Mutex::new(()),
            path: Some(path.to_path_buf()),
        };
        db.run_migration_set(MIGRATIONS, latest_schema_version(), path)?;
        // MM-1c: rows under header names the current policy denies (a name
        // denylisted after it was stored) are removed. Never fails the open.
        match db.purge_denied_header_names() {
            Ok(0) => {}
            Ok(n) => tracing::info!("purged stored headers for {n} denied header name(s)"),
            Err(e) => tracing::warn!("purging denied header names failed: {e}"),
        }
        db.checkpoint_wal_truncate();
        Ok(db)
    }

    /// Best-effort: checkpoint and truncate the WAL file, freeing whatever
    /// disk it grew to. Called once at the end of `open`, right after
    /// migrations run — a migration that rewrites a large table (0038's
    /// `message_fts` rebuild measured ~312 MB of WAL on a 229k-message
    /// profile) would otherwise sit on disk until the app quits: SQLite's
    /// automatic checkpoint is opportunistic (every ~1000 pages by
    /// default), not immediate, and nothing else forces one right after a
    /// single big write burst. Never fails the open: a failed checkpoint
    /// just leaves the WAL as it was, exactly the pre-existing behaviour.
    fn checkpoint_wal_truncate(&self) {
        let conn = self.lock_conn();
        if let Err(e) = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);") {
            tracing::warn!("post-migration wal_checkpoint(TRUNCATE) failed: {e}");
        }
    }

    /// Lock the single connection, recovering from mutex poisoning.
    ///
    /// A panic anywhere under this lock — a provider parse panic unwinding
    /// through a DB call, a rusqlite bug, an `expect` in this crate — poisons
    /// the `Mutex`. Propagating that poison would convert one transient crash
    /// into a dead profile for the rest of the process's life, because every
    /// later call would panic in turn (the 2026-07-11 Gmail pilot bug: one bad
    /// server response bricked the session until the app was restarted).
    ///
    /// Recovery is sound here: SQLite's own state is unaffected by an unwind —
    /// rusqlite rolls an in-flight `Transaction` back on `Drop`, and every
    /// multi-statement write in this crate goes through one — so the connection
    /// handed back is consistent. A half-written *logical* operation is
    /// therefore never observable; at worst the caller's operation is missing,
    /// which is exactly what its `Err` already told them.
    pub(crate) fn lock_conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| {
            tracing::warn!("db connection mutex was poisoned by a prior panic; recovering");
            e.into_inner()
        })
    }

    /// Take [`Database::thread_rebuild_lock`], recovering from poisoning for
    /// the same reason [`Self::lock_conn`] does: it guards no data of its
    /// own (the projection lives in SQLite, and a rebuild that unwound
    /// mid-way rolled its open transaction back), so a panic in one rebuild
    /// must not stop every later one.
    pub(crate) fn lock_thread_rebuild(&self) -> std::sync::MutexGuard<'_, ()> {
        self.thread_rebuild_lock.lock().unwrap_or_else(|e| {
            tracing::warn!("thread rebuild mutex was poisoned by a prior panic; recovering");
            e.into_inner()
        })
    }

    /// Take [`Database::contacts_refresh_lock`], recovering from poisoning
    /// exactly like [`Self::lock_thread_rebuild`] (it guards no data).
    pub(crate) fn lock_contacts_refresh(&self) -> std::sync::MutexGuard<'_, ()> {
        self.contacts_refresh_lock.lock().unwrap_or_else(|e| {
            tracing::warn!("contacts refresh mutex was poisoned by a prior panic; recovering");
            e.into_inner()
        })
    }

    /// A fresh read-only connection to this profile's file, or `None` when
    /// there is no file to open (hand-built test instances) or it fails to
    /// open -- callers then fall back to reading through `conn`. In WAL mode
    /// a read transaction on it sees one consistent snapshot and neither
    /// waits for nor blocks the writer, so long scans run without holding
    /// the shared connection.
    pub(crate) fn open_read_snapshot(&self) -> Option<Connection> {
        let path = self.path.as_ref()?;
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .ok()?;
        conn.busy_timeout(Duration::from_secs(5)).ok()?;
        search_query::register_sql_functions(&conn).ok()?;
        Some(conn)
    }

    /// Apply every migration in `migrations` with `id > applied`, in order.
    /// `supported` is the too-new guard's ceiling — always
    /// `latest_schema_version()` for the real `MIGRATIONS` set; a test
    /// calling this directly with a synthetic slice passes its own ceiling
    /// so the guard doesn't fire on ids the real build genuinely doesn't
    /// know yet.
    ///
    /// Refuses BEFORE any write (beyond the idempotent `CREATE TABLE IF NOT
    /// EXISTS` below, a no-op on an existing profile) if the profile is
    /// already past `supported` — a downgrade scenario. Otherwise, for a
    /// non-empty profile (`applied > 0`) with pending migrations, takes one
    /// backup before the batch, then applies each migration in its own
    /// transaction: a failure rolls that migration back (a plain
    /// transaction drop — see `lock_conn`'s doc comment for why that's
    /// sound) and returns `MigrationFailed` naming it, leaving the watermark
    /// exactly where it was.
    fn run_migration_set(
        &self,
        migrations: &[Migration],
        supported: i64,
        db_path: &Path,
    ) -> Result<(), OpenError> {
        // ids must be strictly increasing within the set being applied: the
        // applied watermark is MAX(id), so ids may never be inserted below
        // the current max — a DB already past that watermark would silently
        // skip them. (Gaps are fine; reserved id 8 lands as id 10 in the
        // parallel AI-slice PR.)
        debug_assert!(
            migrations.windows(2).all(|w| w[0].id < w[1].id),
            "migration ids must be strictly increasing"
        );
        let mut conn = self.lock_conn();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS _migrations (
                 id INTEGER PRIMARY KEY,
                 name TEXT NOT NULL,
                 applied_at TEXT NOT NULL DEFAULT (datetime('now'))
             );",
        )?;
        let applied: i64 =
            conn.query_row("SELECT COALESCE(MAX(id), 0) FROM _migrations", [], |r| {
                r.get(0)
            })?;
        if applied > supported {
            return Err(OpenError::SchemaTooNew {
                found: applied,
                supported,
            });
        }
        // Defence in depth: a profile whose `_migrations` history names a
        // DIFFERENT migration at an id THIS build also knows is not safe to
        // build on top of either way. Never expected from a real release
        // lineage (ids are permanent once shipped — see the doc comment on
        // `MIGRATIONS`), but two dev builds on parallel branches can each
        // claim the same next id for a different migration. Reuses the
        // SchemaTooNew refusal: the remedy is identical (a build whose
        // migration history actually matches this profile's), even though
        // the literal cause here is a foreign lineage, not strictly "newer".
        if let Some(mismatched_id) = Self::find_mismatched_migration_name(&conn, migrations)? {
            return Err(OpenError::SchemaTooNew {
                found: mismatched_id,
                supported,
            });
        }
        let (dir, stem) = Self::backup_dir_and_stem(db_path);
        // Age-based retention, independent of the newest-1 cap in
        // `prune_old_backups` below: a single ~420MB backup carrying
        // deleted mail, kept forever, is its own liability. Runs on every
        // allowed open — not only when a migration actually happens — but
        // never before the two refusals above, so a refused open touches
        // nothing beyond this housekeeping. Best-effort: a failure here
        // must never block opening the profile. See DECISIONS.md.
        if let Err(e) =
            Self::prune_backups_older_than(dir, stem, Duration::from_secs(30 * 24 * 60 * 60))
        {
            tracing::warn!("failed to prune old pre-migration backups: {e}");
        }
        let pending: Vec<&Migration> = migrations.iter().filter(|m| m.id > applied).collect();
        if pending.is_empty() {
            return Ok(());
        }
        // A HEAVY migration (rebuilds a large table from scratch, needing
        // room for the old and new copy at once mid-migration — see
        // `is_heavy_migration`'s doc comment) can need far more headroom
        // than the backup's own 1.2x precheck leaves free afterward: that
        // precheck only reserves room for ITS OWN copy, leaving roughly
        // 0.2x(DB) free once it succeeds (`take_pre_migration_backup`'s doc
        // comment). Check BEFORE taking that backup at all — starting a
        // backup, succeeding, and then hitting SQLITE_FULL partway through
        // the migration is strictly worse than refusing up front: the data
        // stays untouched either way, but the user is left with a launch
        // that never comes up instead of a clear reason and a chance to
        // free space and retry. 1.8x(DB) + the same 100MB floor as the
        // backup precheck: 1.2x for a backup that still runs right after
        // this check, plus roughly the DB's own size again for the heavy
        // migration's WAL growth on top of it.
        if let Some(heavy) = pending.iter().find(|m| is_heavy_migration(m.id)) {
            if applied > 0 {
                let db_size = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
                let needed = db_size
                    .saturating_mul(9)
                    .saturating_div(5)
                    .saturating_add(100 * 1024 * 1024);
                let (dir, _stem) = Self::backup_dir_and_stem(db_path);
                if let Some(free) = free_space_bytes(dir) {
                    if free < needed {
                        let needed_mb = needed.div_ceil(1024 * 1024);
                        tracing::error!(
                            migration_id = heavy.id,
                            migration_name = heavy.name,
                            free_bytes = free,
                            needed_bytes = needed,
                            "refusing to start a heavy migration: not enough free space"
                        );
                        return Err(OpenError::MigrationFailed {
                            id: heavy.id,
                            name: heavy.name.to_string(),
                            reason: format!(
                                "needs about {needed_mb} MB free to upgrade; free some space \
                                 and relaunch"
                            ),
                            backup_path: None,
                        });
                    }
                }
            }
        }
        // Brand-new profile (nothing applied yet): nothing to protect, skip
        // the backup. Otherwise take one, best-effort, before the batch —
        // named after the first migration about to run.
        let backup_path = if applied > 0 {
            match Self::take_pre_migration_backup(&conn, db_path, pending[0].id) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!("pre-migration backup failed, proceeding without one: {e}");
                    None
                }
            }
        } else {
            None
        };
        for m in pending {
            // Every failure from here on — beginning the transaction,
            // running the migration's SQL, recording the watermark, or the
            // COMMIT itself — must become `MigrationFailed`, not bubble up
            // as a generic `OpenError::Io`. A bare `?` on any of these would
            // do that: `CoreError`'s `classify` (Swift) only recognises the
            // two typed prefixes, so an unclassified error falls through to
            // `AppModel`'s unconditional `fatalError` instead of the
            // recovery alert. On a full disk in WAL mode specifically, the
            // failure often only surfaces at COMMIT (the FS runs out of
            // room writing WAL frames), so that path matters as much as the
            // SQL-execution one below.
            let tx = match conn.transaction() {
                Ok(tx) => tx,
                Err(e) => {
                    tracing::error!(
                        migration_id = m.id,
                        migration_name = m.name,
                        "failed to begin migration transaction: {e}"
                    );
                    return Err(OpenError::MigrationFailed {
                        id: m.id,
                        name: m.name.to_string(),
                        reason: e.to_string(),
                        backup_path,
                    });
                }
            };
            if let Err(e) = tx.execute_batch(m.sql) {
                // `tx` drops here uncommitted -> rolled back; the watermark
                // (and everything else) is exactly as it was before this
                // migration was attempted. The full technical reason is
                // logged here (Rust's own log file, see `logging.rs`) since
                // `OpenError::MigrationFailed`'s `Display` — what the user
                // actually sees — deliberately omits it.
                tracing::error!(
                    migration_id = m.id,
                    migration_name = m.name,
                    "migration failed, rolled back: {e}"
                );
                return Err(OpenError::MigrationFailed {
                    id: m.id,
                    name: m.name.to_string(),
                    reason: e.to_string(),
                    backup_path,
                });
            }
            if let Err(e) = tx.execute(
                "INSERT INTO _migrations (id, name) VALUES (?1, ?2)",
                rusqlite::params![m.id, m.name],
            ) {
                tracing::error!(
                    migration_id = m.id,
                    migration_name = m.name,
                    "failed to record migration watermark, rolled back: {e}"
                );
                return Err(OpenError::MigrationFailed {
                    id: m.id,
                    name: m.name.to_string(),
                    reason: e.to_string(),
                    backup_path,
                });
            }
            if let Err(e) = tx.commit() {
                tracing::error!(
                    migration_id = m.id,
                    migration_name = m.name,
                    "migration failed at commit, rolled back: {e}"
                );
                return Err(OpenError::MigrationFailed {
                    id: m.id,
                    name: m.name.to_string(),
                    reason: e.to_string(),
                    backup_path,
                });
            }
        }
        Ok(())
    }

    /// The applied `(id, name)` pairs among `_migrations` rows whose id is
    /// also present in `migrations` (this build's known set) but names a
    /// DIFFERENT migration. `None` when every overlapping id agrees. See the
    /// call site in `run_migration_set` for why this is checked at all.
    fn find_mismatched_migration_name(
        conn: &Connection,
        migrations: &[Migration],
    ) -> Result<Option<i64>, OpenError> {
        let mut stmt = conn.prepare("SELECT id, name FROM _migrations")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let name: String = row.get(1)?;
            if let Some(m) = migrations.iter().find(|m| m.id == id) {
                if m.name != name {
                    return Ok(Some(id));
                }
            }
        }
        Ok(None)
    }

    /// The backup directory and filename stem for `db_path` — shared by
    /// `take_pre_migration_backup` and the retention helpers so they always
    /// agree on which files are "this profile's backups".
    fn backup_dir_and_stem(db_path: &Path) -> (&Path, &str) {
        let dir = db_path.parent().unwrap_or_else(|| Path::new("."));
        let stem = db_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("castellum.db");
        (dir, stem)
    }

    /// `VACUUM INTO` a snapshot of `db_path` next to it, named
    /// `<file>.pre-<id>.bak`, then prune to the newest 1 (see DECISIONS.md —
    /// a single backup, aged out after 30 days regardless, is the retention
    /// decision; two ~420MB copies carrying deleted mail forever was too
    /// much). Returns the backup's path on success.
    ///
    /// Two failure modes this guards against, both realistic on a full
    /// disk — the scenario this whole backup exists to protect data
    /// through, not just the happy path:
    ///
    /// - a `VACUUM INTO` that runs out of room partway leaves a partial
    ///   file (and possibly a `-journal` sidecar) behind. Writing to a
    ///   `.bak.partial` name and only `rename`-ing to the real `.bak` name
    ///   on success means retention (which only ever looks at `.bak` files)
    ///   can never mistake a partial for a complete backup and prune a good
    ///   one to keep it; on failure both are deleted so they don't go on
    ///   eating the last free space either.
    /// - taking the backup at all can itself be what tips a near-full disk
    ///   over the edge and starves the migration that follows it of the
    ///   room it needs. A best-effort free-space precheck skips the backup
    ///   (never the migration) when the profile volume doesn't have
    ///   comfortable headroom for one.
    fn take_pre_migration_backup(
        conn: &Connection,
        db_path: &Path,
        next_id: i64,
    ) -> Result<String, String> {
        let (dir, stem) = Self::backup_dir_and_stem(db_path);
        let final_path = dir.join(format!("{stem}.pre-{next_id}.bak"));
        let partial_path = dir.join(format!("{stem}.pre-{next_id}.bak.partial"));
        let partial_journal = journal_sidecar(&partial_path);

        let db_size = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
        // `VACUUM INTO` writes a full compacted copy, so require the DB
        // size plus a 20% margin plus a flat 100MB floor (small profiles
        // still need headroom for the migration itself, which runs right
        // after this) before even attempting a backup.
        let needed = db_size
            .saturating_mul(6)
            .saturating_div(5)
            .saturating_add(100 * 1024 * 1024);
        if let Some(free) = free_space_bytes(dir) {
            if free < needed {
                return Err(format!(
                    "skipped: only {free} bytes free on the profile volume, need at least \
                     {needed} for a safe backup"
                ));
            }
        }

        // Clean up any leftovers from a previous crashed/failed attempt at
        // this id — `VACUUM INTO` refuses to overwrite an existing file,
        // and a stale partial must never masquerade as a complete backup.
        remove_backup_artifact(&partial_path);
        remove_backup_artifact(&partial_journal);
        let _ = std::fs::remove_file(&final_path);

        let partial_str = partial_path.to_string_lossy().into_owned();
        if let Err(e) = conn.execute("VACUUM INTO ?1", rusqlite::params![partial_str]) {
            // The partial file (and its rollback journal, if the failure
            // left one) must not survive a failed attempt: on a full disk
            // that's exactly the space the migration needs next.
            remove_backup_artifact(&partial_path);
            remove_backup_artifact(&partial_journal);
            return Err(e.to_string());
        }
        // Promote atomically: retention (`prune_old_backups`,
        // `prune_backups_older_than`) only ever looks at `.bak` files, so a
        // backup is invisible to it — and can never be treated as "the"
        // backup by `OpenError::MigrationFailed` — until this succeeds.
        std::fs::rename(&partial_path, &final_path).map_err(|e| e.to_string())?;
        Self::prune_old_backups(dir, stem).map_err(|e| e.to_string())?;
        Ok(final_path.to_string_lossy().into_owned())
    }

    /// Keep only the single most-recently-modified `<stem>.pre-*.bak` file
    /// in `dir`. Age-based pruning (`prune_backups_older_than`) is the
    /// other half of retention — this caps count, that caps age; see
    /// DECISIONS.md.
    fn prune_old_backups(dir: &Path, stem: &str) -> std::io::Result<()> {
        let prefix = format!("{stem}.pre-");
        let mut backups: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(&prefix) && name.ends_with(".bak") {
                let mtime = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                backups.push((mtime, entry.path()));
            }
        }
        backups.sort_by_key(|(t, _)| *t);
        while backups.len() > 1 {
            let (_, path) = backups.remove(0);
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }

    /// Delete `<stem>.pre-*.bak` files in `dir` last modified more than
    /// `max_age` ago — independent of, and in addition to, the newest-1 cap
    /// in `prune_old_backups`. Runs on every allowed `open()` (see the call
    /// site in `run_migration_set`), not only when a migration actually
    /// happens, so a profile that stays on the same schema for months still
    /// ages its one backup out rather than keeping it (and the deleted mail
    /// inside it) forever.
    fn prune_backups_older_than(dir: &Path, stem: &str, max_age: Duration) -> std::io::Result<()> {
        let prefix = format!("{stem}.pre-");
        let now = std::time::SystemTime::now();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with(&prefix) && name.ends_with(".bak")) {
                continue;
            }
            let mtime = match entry.metadata().and_then(|m| m.modified()) {
                Ok(t) => t,
                Err(_) => continue,
            };
            if now.duration_since(mtime).unwrap_or(Duration::ZERO) > max_age {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        Ok(())
    }

    /// The current applied schema version (highest applied migration id, 0 if none).
    pub fn schema_version(&self) -> Result<i64, DbError> {
        let conn = self.lock_conn();
        let v: i64 = conn.query_row("SELECT COALESCE(MAX(id), 0) FROM _migrations", [], |r| {
            r.get(0)
        })?;
        Ok(v)
    }

    /// Cheaply report how many migrations are pending for the profile at
    /// `path`, WITHOUT running any of them and without needing the
    /// exclusive/writable access `open` takes — opens the database
    /// `SQLITE_OPEN_READ_ONLY` when the file already exists. Backs the
    /// Swift launch preflight's decision to show the
    /// "Upgrading your mail database…" window BEFORE calling the real,
    /// migrating [`Database::open`].
    ///
    /// - A profile that doesn't exist yet (first-ever launch) reports every
    ///   migration as pending: `open` creates and migrates it from empty,
    ///   which is fast enough to need no window, but the count itself is
    ///   still accurate (nothing has been applied).
    /// - Reuses [`OpenError::SchemaTooNew`] for a profile whose applied
    ///   schema is already ahead of this build, so the existing #169
    ///   refusal (recovery alert, no migrating open ever attempted) still
    ///   wins — this probe never tries to "fix" that by opening for real,
    ///   it only reports the same refusal `open` would.
    /// - Never writes: even the idempotent `CREATE TABLE IF NOT EXISTS
    ///   _migrations` that `run_migration_set` performs is skipped here; a
    ///   database file that exists but has no `_migrations` table yet
    ///   (never migrated) reads as 0 applied, the same starting point
    ///   `run_migration_set` sees before it creates that table.
    pub fn pending_migration_count(path: &Path) -> Result<PendingMigrations, OpenError> {
        if !path.exists() {
            return Ok(PendingMigrations {
                applied_version: 0,
                pending_count: MIGRATIONS.len() as i64,
                has_heavy: MIGRATIONS.iter().any(|m| is_heavy_migration(m.id)),
            });
        }
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        // `.unwrap_or(0)` also covers "no such table" (a DB file that
        // exists but was never migrated) — same as the missing-file case
        // above, nothing applied yet.
        let applied: i64 = conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM _migrations", [], |r| {
                r.get(0)
            })
            .unwrap_or(0);
        let supported = latest_schema_version();
        if applied > supported {
            return Err(OpenError::SchemaTooNew {
                found: applied,
                supported,
            });
        }
        let pending_count = MIGRATIONS.iter().filter(|m| m.id > applied).count() as i64;
        let has_heavy = MIGRATIONS
            .iter()
            .any(|m| m.id > applied && is_heavy_migration(m.id));
        Ok(PendingMigrations {
            applied_version: applied,
            pending_count,
            has_heavy,
        })
    }

    /// Test-only helper: hard-delete a contact by id so enrichment cascade tests
    /// can exercise ON DELETE CASCADE without needing the full sync/refresh cycle.
    #[cfg(test)]
    pub(crate) fn delete_contact_for_test(&self, id: i64) -> Result<(), DbError> {
        let conn = self.lock_conn();
        conn.execute("DELETE FROM contacts WHERE id = ?1", rusqlite::params![id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_db() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        // Unique per RUN, not just per pid. These files are never deleted and
        // pids recycle, so keying on pid + counter alone reopens an EARLIER
        // run's database — 109,704 of them had accumulated in $TMPDIR, and a
        // test asserting on row counts saw rows that were not its own. It
        // presents as a flake and is not one.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        castellum_testutil::scratch_root().join(format!(
            "castellum-db-test-{}-{stamp}-{n}.db",
            std::process::id()
        ))
    }

    /// A panic anywhere under the connection lock (a provider parse panic, a
    /// rusqlite bug, an `expect` in this crate) poisons the `Mutex`. Poisoning
    /// must NOT brick the profile: the 2026-07-11 Gmail pilot crash left every
    /// later DB call panicking on `expect("db mutex poisoned")`, so one bad
    /// server response killed the session until the app was restarted.
    #[test]
    fn a_panic_under_the_connection_lock_does_not_brick_the_database() {
        use std::sync::Arc;

        let path = tmp_db();
        let db = Arc::new(Database::open(&path).expect("open"));
        let before = db.schema_version().expect("read version before poisoning");

        // Poison the mutex exactly as a panicking sync path would.
        let poisoner = Arc::clone(&db);
        let crashed = std::thread::spawn(move || {
            let _guard = poisoner.lock_conn();
            panic!("simulated sync-path panic while holding the DB lock");
        })
        .join();
        assert!(crashed.is_err(), "the helper thread must have panicked");
        assert!(
            db.conn.is_poisoned(),
            "the mutex must actually be poisoned for this test to mean anything"
        );

        // The profile must still be usable — reads and writes both.
        assert_eq!(
            db.schema_version().expect("read version after poisoning"),
            before,
            "schema version still readable through a poisoned mutex"
        );
        {
            let conn = db.lock_conn();
            conn.execute_batch("CREATE TABLE poison_probe (id INTEGER PRIMARY KEY);")
                .expect("writes still work after poisoning");
        }

        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
    }

    /// The safety argument for recovering from mutex poisoning is that an
    /// unwind cannot leave a half-written logical operation visible: every
    /// multi-statement write here runs inside a `rusqlite::Transaction`, whose
    /// `Drop` ROLLBACKs before the `MutexGuard` is released. Assert that
    /// directly rather than only in a comment.
    #[test]
    fn a_panic_inside_an_open_transaction_rolls_back_before_the_lock_is_released() {
        use std::sync::Arc;

        let path = tmp_db();
        let db = Arc::new(Database::open(&path).expect("open"));
        {
            let conn = db.lock_conn();
            conn.execute_batch("CREATE TABLE poison_tx_probe (id INTEGER PRIMARY KEY);")
                .expect("probe table");
        }

        let poisoner = Arc::clone(&db);
        let crashed = std::thread::spawn(move || {
            let mut conn = poisoner.lock_conn();
            let tx = conn.transaction().expect("begin");
            tx.execute("INSERT INTO poison_tx_probe (id) VALUES (1)", [])
                .expect("insert");
            // Unwind with the transaction still OPEN and uncommitted.
            panic!("simulated panic mid-transaction");
        })
        .join();
        assert!(crashed.is_err(), "the helper thread must have panicked");
        assert!(db.conn.is_poisoned());

        let conn = db.lock_conn();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM poison_tx_probe", [], |r| r.get(0))
            .expect("still queryable through the poisoned mutex");
        assert_eq!(
            rows, 0,
            "the uncommitted transaction must have rolled back — recovery never \
             exposes a half-written operation"
        );
        drop(conn);

        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
    }

    #[test]
    fn open_runs_migrations_to_latest() {
        let path = tmp_db();
        let db = Database::open(&path).expect("open");
        assert_eq!(db.schema_version().unwrap(), latest_schema_version());
        assert!(latest_schema_version() >= 1);
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reopen_is_idempotent() {
        let path = tmp_db();
        let v1 = {
            let db = Database::open(&path).expect("first open");
            db.schema_version().unwrap()
        };
        let db2 = Database::open(&path).expect("reopen");
        assert_eq!(
            db2.schema_version().unwrap(),
            v1,
            "no re-application on reopen"
        );
        // The migrations ran exactly once (count matches the set, which may
        // have id gaps, so compare against MIGRATIONS.len(), not MAX(id)).
        {
            let conn = db2.lock_conn();
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM _migrations", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count as usize, MIGRATIONS.len());
        }
        drop(db2);
        // WAL leaves -wal/-shm sidecars; clean them too.
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
    }

    #[test]
    fn migration_set_is_ordered_unique_and_matches_its_sql_files() {
        // Strictly ascending ids: `run_migration_set` only debug_asserts it,
        // and the applied watermark is MAX(id), so an id slotted in below an
        // earlier one would be silently skipped on every existing profile.
        assert!(
            MIGRATIONS.windows(2).all(|w| w[0].id < w[1].id),
            "migration ids must be strictly ascending"
        );
        let mut names: Vec<&str> = MIGRATIONS.iter().map(|m| m.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            MIGRATIONS.len(),
            "migration names must be unique"
        );
        assert_eq!(latest_schema_version(), MIGRATIONS.last().unwrap().id);

        // Every `src/migrations/NNNN_name.sql` is wired in under id NNNN and
        // name `name`, with exactly that file's SQL: an orphaned or
        // renumbered file is a migration that never runs.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/migrations");
        let mut files = 0;
        for entry in std::fs::read_dir(&dir).expect("read migrations dir") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("sql") {
                continue;
            }
            files += 1;
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .expect("utf-8 name");
            let (num, name) = stem.split_once('_').expect("NNNN_name.sql");
            let id: i64 = num.parse().expect("numeric prefix");
            let m = MIGRATIONS
                .iter()
                .find(|m| m.id == id)
                .unwrap_or_else(|| panic!("{stem}.sql is not in MIGRATIONS"));
            assert_eq!(m.name, name, "{stem}.sql is wired in under another name");
            assert_eq!(
                m.sql,
                std::fs::read_to_string(&path).expect("read sql"),
                "{stem}.sql differs from migration {id}'s SQL"
            );
        }
        assert_eq!(
            files,
            MIGRATIONS.len() - 1,
            "every migration but the inline id 1 has a file"
        );
    }

    #[test]
    fn rerunning_the_migration_set_leaves_the_schema_untouched() {
        let path = tmp_db();
        let db = Database::open(&path).expect("open");
        let schema = |db: &Database| -> Vec<(String, String)> {
            let conn = db.lock_conn();
            let mut stmt = conn
                .prepare("SELECT name, COALESCE(sql, '') FROM sqlite_master ORDER BY type, name")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        let before = schema(&db);
        db.run_migration_set(MIGRATIONS, latest_schema_version(), &path)
            .expect("second run of the full set is a no-op");
        assert_eq!(schema(&db), before);
        assert_eq!(db.schema_version().unwrap(), latest_schema_version());
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
    }

    #[test]
    fn pre_0016_profile_upgrades_cleanly_on_reopen() {
        // Simulate a v15 profile in the wild: drop the 0016 tables and the
        // 0017 objects (a v15 profile predates both; 0017's backfill needs
        // the legacy join tables 0011 created), then reopen — the runner
        // must re-apply 0016 and 0017 in order.
        let path = tmp_db();
        {
            let db = Database::open(&path).expect("first open");
            let conn = db.lock_conn();
            conn.execute_batch(
                "DROP TABLE mailbox_sync_state;
                 DROP TABLE account_sync_state;
                 DROP TRIGGER links_work_item_delete;
                 DROP TABLE work_items;
                 DROP TRIGGER links_project_delete;
                 DROP TRIGGER links_message_delete;
                 DROP TRIGGER links_contact_delete;
                 DROP TABLE links;
                 ALTER TABLE accounts DROP COLUMN write_enabled;
                 ALTER TABLE operations DROP COLUMN not_before_epoch;
                 ALTER TABLE message_attachments DROP COLUMN section;
                 ALTER TABLE message_attachments DROP COLUMN content_id;
                 ALTER TABLE message_attachments DROP COLUMN is_inline;
                 ALTER TABLE message_attachments DROP COLUMN transfer_encoding;
                 DROP TABLE attachment_cache;
                 DROP TABLE snoozed_messages;
                 ALTER TABLE drafts DROP COLUMN body_html;
                 ALTER TABLE drafts DROP COLUMN from_email;
                 ALTER TABLE drafts DROP COLUMN from_name;
                 ALTER TABLE drafts DROP COLUMN server_mailbox_path;
                 ALTER TABLE drafts DROP COLUMN server_uid;
                 ALTER TABLE drafts DROP COLUMN server_uid_validity;
                 ALTER TABLE drafts DROP COLUMN server_message_id;
                 ALTER TABLE drafts DROP COLUMN server_saved_at_epoch;
                 DROP TABLE account_identities;
                 CREATE TABLE project_emails (
                     project_id TEXT NOT NULL,
                     message_id TEXT NOT NULL,
                     linked_at  INTEGER NOT NULL,
                     PRIMARY KEY (project_id, message_id)
                 );
                 CREATE TABLE project_contacts (
                     project_id TEXT NOT NULL,
                     contact_id INTEGER NOT NULL,
                     origin     TEXT NOT NULL DEFAULT 'manual',
                     linked_at  INTEGER NOT NULL,
                     PRIMARY KEY (project_id, contact_id)
                 );
                 INSERT INTO project_emails VALUES ('pv15', 'acc:INBOX:9', 990);
                 INSERT INTO project_contacts VALUES ('pv15', 3, 'auto', 991);
                 DROP INDEX idx_mailboxes_role;
                 DROP INDEX idx_messages_sent_at;
                 ALTER TABLE messages DROP COLUMN list_unsubscribe_mailto;
                 ALTER TABLE messages DROP COLUMN list_unsubscribe_url;
                 ALTER TABLE messages DROP COLUMN list_unsubscribe_one_click;
                 DROP TABLE smart_mailboxes;
                 DROP TABLE message_trust;
                 DROP TABLE sender_remote_content_policy;
                 DROP TABLE draft_attachment_blobs;
                 DROP TABLE draft_attachments;
                 DROP TABLE message_embeddings;
                 DROP TABLE calendar_invites;
                 DROP TABLE thread_extractions;
                 DROP TABLE saved_filters;
                 DROP TABLE tag_definitions;
                 DROP TABLE message_keywords;
                 DROP INDEX idx_messages_account_rfc822;
                 DROP TABLE work_item_provenance;
                 DROP TABLE notification_outbox;
                 DROP TABLE vip_senders;
                 DROP TABLE thread_mutes;
                 ALTER TABLE threads DROP COLUMN is_muted;
                 DROP INDEX idx_message_addresses_lower_address;
                 DROP TABLE thread_rebuild_state;
                 DROP TABLE thread_dirty_messages;
                 DROP TRIGGER messages_before_delete_fts;
                 DROP TABLE message_fts_rowid;
                 DROP TABLE message_fts;
                 CREATE VIRTUAL TABLE message_fts USING fts5(
                     message_id UNINDEXED, subject, sender, body, tokenize = 'unicode61');
                 CREATE TRIGGER messages_after_delete_fts AFTER DELETE ON messages BEGIN
                     DELETE FROM message_fts WHERE message_id = OLD.id;
                 END;
                 ALTER TABLE sync_cursors DROP COLUMN descriptor_checkpoint_uid;
                 DROP TABLE message_decode_skips;
                 DROP TABLE suggestions;
                 DROP TABLE saved_views;
                 DROP TABLE project_timeline;
                 DROP TABLE pending_refetch;
                 DROP TABLE pending_rules;
                 DROP TABLE message_header_capture;
                 DROP TABLE message_headers;
                 DROP TABLE header_names;
                 DELETE FROM _migrations WHERE id IN (16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57);",
            )
            .unwrap();
        }
        let db = Database::open(&path).expect("upgrade reopen");
        assert_eq!(db.schema_version().unwrap(), latest_schema_version());
        {
            let conn = db.lock_conn();
            let tables: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table'
                     AND name IN ('account_sync_state', 'mailbox_sync_state')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(tables, 2, "0016 tables recreated by the upgrade");
            // The v15 data survived the 0016 + 0017 chain: backfilled links,
            // legacy tables gone.
            let backfilled: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM links WHERE src_id = 'pv15'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(backfilled, 2, "v15 rows backfilled through the chain");
            let legacy: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table'
                     AND name IN ('project_emails', 'project_contacts')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(legacy, 0, "legacy join tables dropped after the chain");
        }
        // And the upgraded profile records diagnostics normally.
        db.record_account_sync_attempt("acct").unwrap();
        assert!(db.get_account_sync_state("acct").unwrap().is_some());
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
    }

    #[test]
    fn pre_0017_profile_backfills_project_links() {
        // Simulate a v16 profile in the wild: recreate the legacy join tables
        // with data, drop the 0017 objects and its watermark row, then reopen.
        // The migration must backfill `links` (origins mapped, timestamps
        // preserved) and drop the legacy tables.
        let path = tmp_db();
        {
            let db = Database::open(&path).expect("first open");
            let conn = db.lock_conn();
            conn.execute_batch(
                "DROP TRIGGER links_work_item_delete;
                 DROP TABLE work_items;
                 DROP TRIGGER links_project_delete;
                 DROP TRIGGER links_message_delete;
                 DROP TRIGGER links_contact_delete;
                 DROP TABLE links;
                 ALTER TABLE accounts DROP COLUMN write_enabled;
                 ALTER TABLE operations DROP COLUMN not_before_epoch;
                 DROP INDEX idx_mailboxes_role;
                 DROP INDEX idx_messages_sent_at;
                 DROP TABLE draft_attachment_blobs;
                 DROP TABLE draft_attachments;
                 ALTER TABLE message_attachments DROP COLUMN section;
                 ALTER TABLE message_attachments DROP COLUMN content_id;
                 ALTER TABLE message_attachments DROP COLUMN is_inline;
                 ALTER TABLE message_attachments DROP COLUMN transfer_encoding;
                 DROP TABLE attachment_cache;
                 DROP TABLE snoozed_messages;
                 ALTER TABLE drafts DROP COLUMN body_html;
                 ALTER TABLE drafts DROP COLUMN from_email;
                 ALTER TABLE drafts DROP COLUMN from_name;
                 ALTER TABLE drafts DROP COLUMN server_mailbox_path;
                 ALTER TABLE drafts DROP COLUMN server_uid;
                 ALTER TABLE drafts DROP COLUMN server_uid_validity;
                 ALTER TABLE drafts DROP COLUMN server_message_id;
                 ALTER TABLE drafts DROP COLUMN server_saved_at_epoch;
                 DROP TABLE account_identities;
                 ALTER TABLE messages DROP COLUMN list_unsubscribe_mailto;
                 ALTER TABLE messages DROP COLUMN list_unsubscribe_url;
                 ALTER TABLE messages DROP COLUMN list_unsubscribe_one_click;
                 DROP TABLE smart_mailboxes;
                 DROP TABLE message_trust;
                 DROP TABLE sender_remote_content_policy;
                 DROP TABLE message_embeddings;
                 DROP TABLE calendar_invites;
                 DROP TABLE thread_extractions;
                 DROP TABLE saved_filters;
                 DROP TABLE tag_definitions;
                 DROP TABLE message_keywords;
                 DROP INDEX idx_messages_account_rfc822;
                 DROP TABLE work_item_provenance;
                 DROP TABLE notification_outbox;
                 DROP TABLE vip_senders;
                 DROP TABLE thread_mutes;
                 ALTER TABLE threads DROP COLUMN is_muted;
                 DROP INDEX idx_message_addresses_lower_address;
                 DROP TABLE thread_rebuild_state;
                 DROP TABLE thread_dirty_messages;
                 DROP TRIGGER messages_before_delete_fts;
                 DROP TABLE message_fts_rowid;
                 DROP TABLE message_fts;
                 CREATE VIRTUAL TABLE message_fts USING fts5(
                     message_id UNINDEXED, subject, sender, body, tokenize = 'unicode61');
                 CREATE TRIGGER messages_after_delete_fts AFTER DELETE ON messages BEGIN
                     DELETE FROM message_fts WHERE message_id = OLD.id;
                 END;
                 ALTER TABLE sync_cursors DROP COLUMN descriptor_checkpoint_uid;
                 DROP TABLE message_decode_skips;
                 DROP TABLE suggestions;
                 DROP TABLE saved_views;
                 DROP TABLE project_timeline;
                 DROP TABLE pending_refetch;
                 DROP TABLE pending_rules;
                 DROP TABLE message_header_capture;
                 DROP TABLE message_headers;
                 DROP TABLE header_names;
                 DELETE FROM _migrations WHERE id IN (17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57);
                 CREATE TABLE project_emails (
                     project_id TEXT NOT NULL,
                     message_id TEXT NOT NULL,
                     linked_at  INTEGER NOT NULL,
                     PRIMARY KEY (project_id, message_id)
                 );
                 CREATE TABLE project_contacts (
                     project_id TEXT NOT NULL,
                     contact_id INTEGER NOT NULL,
                     origin     TEXT NOT NULL DEFAULT 'manual',
                     linked_at  INTEGER NOT NULL,
                     PRIMARY KEY (project_id, contact_id)
                 );
                 INSERT INTO project_emails VALUES ('p1', 'acc:INBOX:1', 111);
                 INSERT INTO project_contacts VALUES ('p1', 7, 'manual', 222);
                 INSERT INTO project_contacts VALUES ('p1', 8, 'auto', 333);",
            )
            .unwrap();
        }
        let db = Database::open(&path).expect("upgrade reopen");
        assert_eq!(db.schema_version().unwrap(), latest_schema_version());
        {
            let conn = db.lock_conn();
            let legacy: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table'
                     AND name IN ('project_emails', 'project_contacts')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(legacy, 0, "legacy join tables dropped");
            let (origin, prov, created): (String, Option<String>, i64) = conn
                .query_row(
                    "SELECT origin, provenance, created_at FROM links
                     WHERE dst_type='contact' AND dst_id='8'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            assert_eq!(origin, "derived", "legacy 'auto' maps to 'derived'");
            assert_eq!(prov.as_deref(), Some("participant"));
            assert_eq!(created, 333, "linked_at preserved");
            let manual_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM links WHERE origin='manual'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(manual_count, 2, "email link + manual contact link");
        }
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
    }

    #[test]
    fn pre_0019_profile_defaults_existing_accounts_to_read_only() {
        // A v18 profile in the wild: drop 0019's column + watermark and seed an
        // account that predates the write policy, then reopen. The upgrade must
        // re-add write_enabled defaulting to 0 so the EXISTING account becomes
        // read-only (global fail-closed on upgrade).
        let path = tmp_db();
        {
            let db = Database::open(&path).expect("first open");
            let conn = db.lock_conn();
            conn.execute_batch(
                "ALTER TABLE accounts DROP COLUMN write_enabled;
                 ALTER TABLE operations DROP COLUMN not_before_epoch;
                 DROP INDEX idx_mailboxes_role;
                 DROP INDEX idx_messages_sent_at;
                 DROP TABLE draft_attachment_blobs;
                 DROP TABLE draft_attachments;
                 ALTER TABLE message_attachments DROP COLUMN section;
                 ALTER TABLE message_attachments DROP COLUMN content_id;
                 ALTER TABLE message_attachments DROP COLUMN is_inline;
                 ALTER TABLE message_attachments DROP COLUMN transfer_encoding;
                 DROP TABLE attachment_cache;
                 DROP TABLE snoozed_messages;
                 ALTER TABLE drafts DROP COLUMN body_html;
                 ALTER TABLE drafts DROP COLUMN from_email;
                 ALTER TABLE drafts DROP COLUMN from_name;
                 ALTER TABLE drafts DROP COLUMN server_mailbox_path;
                 ALTER TABLE drafts DROP COLUMN server_uid;
                 ALTER TABLE drafts DROP COLUMN server_uid_validity;
                 ALTER TABLE drafts DROP COLUMN server_message_id;
                 ALTER TABLE drafts DROP COLUMN server_saved_at_epoch;
                 DROP TABLE account_identities;
                 ALTER TABLE messages DROP COLUMN list_unsubscribe_mailto;
                 ALTER TABLE messages DROP COLUMN list_unsubscribe_url;
                 ALTER TABLE messages DROP COLUMN list_unsubscribe_one_click;
                 DROP TABLE smart_mailboxes;
                 DROP TABLE message_trust;
                 DROP TABLE sender_remote_content_policy;
                 DROP TABLE message_embeddings;
                 DROP TABLE calendar_invites;
                 DROP TABLE thread_extractions;
                 DROP TABLE saved_filters;
                 DROP TABLE tag_definitions;
                 DROP TABLE message_keywords;
                 DROP INDEX idx_messages_account_rfc822;
                 DROP TABLE work_item_provenance;
                 DROP TABLE notification_outbox;
                 DROP TABLE vip_senders;
                 DROP TABLE thread_mutes;
                 ALTER TABLE threads DROP COLUMN is_muted;
                 DROP INDEX idx_message_addresses_lower_address;
                 DROP TABLE thread_rebuild_state;
                 DROP TABLE thread_dirty_messages;
                 DROP TRIGGER messages_before_delete_fts;
                 DROP TABLE message_fts_rowid;
                 DROP TABLE message_fts;
                 CREATE VIRTUAL TABLE message_fts USING fts5(
                     message_id UNINDEXED, subject, sender, body, tokenize = 'unicode61');
                 CREATE TRIGGER messages_after_delete_fts AFTER DELETE ON messages BEGIN
                     DELETE FROM message_fts WHERE message_id = OLD.id;
                 END;
                 ALTER TABLE sync_cursors DROP COLUMN descriptor_checkpoint_uid;
                 DROP TABLE message_decode_skips;
                 DROP TABLE work_items;
                 CREATE TABLE work_items (
                     id TEXT PRIMARY KEY,
                     project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                     title TEXT NOT NULL,
                     notes TEXT NOT NULL DEFAULT '',
                     state TEXT NOT NULL DEFAULT 'needs_reply'
                         CHECK (state IN ('needs_reply','waiting','delegated','blocked','done','someday')),
                     position INTEGER NOT NULL DEFAULT 0,
                     created_at INTEGER NOT NULL,
                     updated_at INTEGER NOT NULL
                 );
                 CREATE INDEX idx_work_items_project ON work_items(project_id, state, position);
                 CREATE TRIGGER links_work_item_delete AFTER DELETE ON work_items BEGIN
                     DELETE FROM links WHERE (src_type = 'work_item' AND src_id = OLD.id)
                                          OR (dst_type = 'work_item' AND dst_id = OLD.id);
                 END;
                 DROP TABLE suggestions;
                 DROP TABLE saved_views;
                 DROP TABLE project_timeline;
                 DROP TABLE pending_refetch;
                 DROP TABLE pending_rules;
                 DROP TABLE message_header_capture;
                 DROP TABLE message_headers;
                 DROP TABLE header_names;
                 DELETE FROM _migrations WHERE id IN (19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57);
                 INSERT INTO accounts (id, email, provider) VALUES ('old', 'o@x.test', 'imap');",
            )
            .unwrap();
        }
        let db = Database::open(&path).expect("upgrade reopen");
        assert_eq!(db.schema_version().unwrap(), latest_schema_version());
        assert!(
            !db.account_write_enabled("old").unwrap(),
            "an account that predates the policy is read-only after upgrade"
        );
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
    }

    #[test]
    fn pre_0020_profile_upgrades_cleanly_and_gains_the_undo_send_column() {
        // A v19 profile in the wild: drop 0020's column + watermark, then
        // reopen. The upgrade must re-add `not_before_epoch` (nullable, so
        // every existing queued op reads back due-immediately, unchanged).
        let path = tmp_db();
        {
            let db = Database::open(&path).expect("first open");
            let conn = db.lock_conn();
            conn.execute_batch(
                "ALTER TABLE operations DROP COLUMN not_before_epoch;
                 DROP INDEX idx_mailboxes_role;
                 DROP INDEX idx_messages_sent_at;
                 DROP TABLE draft_attachment_blobs;
                 DROP TABLE draft_attachments;
                 ALTER TABLE message_attachments DROP COLUMN section;
                 ALTER TABLE message_attachments DROP COLUMN content_id;
                 ALTER TABLE message_attachments DROP COLUMN is_inline;
                 ALTER TABLE message_attachments DROP COLUMN transfer_encoding;
                 DROP TABLE attachment_cache;
                 DROP TABLE snoozed_messages;
                 ALTER TABLE drafts DROP COLUMN body_html;
                 ALTER TABLE drafts DROP COLUMN from_email;
                 ALTER TABLE drafts DROP COLUMN from_name;
                 ALTER TABLE drafts DROP COLUMN server_mailbox_path;
                 ALTER TABLE drafts DROP COLUMN server_uid;
                 ALTER TABLE drafts DROP COLUMN server_uid_validity;
                 ALTER TABLE drafts DROP COLUMN server_message_id;
                 ALTER TABLE drafts DROP COLUMN server_saved_at_epoch;
                 DROP TABLE account_identities;
                 ALTER TABLE messages DROP COLUMN list_unsubscribe_mailto;
                 ALTER TABLE messages DROP COLUMN list_unsubscribe_url;
                 ALTER TABLE messages DROP COLUMN list_unsubscribe_one_click;
                 DROP TABLE smart_mailboxes;
                 DROP TABLE message_trust;
                 DROP TABLE sender_remote_content_policy;
                 DROP TABLE message_embeddings;
                 DROP TABLE calendar_invites;
                 DROP TABLE thread_extractions;
                 DROP TABLE saved_filters;
                 DROP TABLE tag_definitions;
                 DROP TABLE message_keywords;
                 DROP INDEX idx_messages_account_rfc822;
                 DROP TABLE work_item_provenance;
                 DROP TABLE notification_outbox;
                 DROP TABLE vip_senders;
                 DROP TABLE thread_mutes;
                 ALTER TABLE threads DROP COLUMN is_muted;
                 DROP INDEX idx_message_addresses_lower_address;
                 DROP TABLE thread_rebuild_state;
                 DROP TABLE thread_dirty_messages;
                 DROP TRIGGER messages_before_delete_fts;
                 DROP TABLE message_fts_rowid;
                 DROP TABLE message_fts;
                 CREATE VIRTUAL TABLE message_fts USING fts5(
                     message_id UNINDEXED, subject, sender, body, tokenize = 'unicode61');
                 CREATE TRIGGER messages_after_delete_fts AFTER DELETE ON messages BEGIN
                     DELETE FROM message_fts WHERE message_id = OLD.id;
                 END;
                 ALTER TABLE sync_cursors DROP COLUMN descriptor_checkpoint_uid;
                 DROP TABLE message_decode_skips;
                 DROP TABLE work_items;
                 CREATE TABLE work_items (
                     id TEXT PRIMARY KEY,
                     project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                     title TEXT NOT NULL,
                     notes TEXT NOT NULL DEFAULT '',
                     state TEXT NOT NULL DEFAULT 'needs_reply'
                         CHECK (state IN ('needs_reply','waiting','delegated','blocked','done','someday')),
                     position INTEGER NOT NULL DEFAULT 0,
                     created_at INTEGER NOT NULL,
                     updated_at INTEGER NOT NULL
                 );
                 CREATE INDEX idx_work_items_project ON work_items(project_id, state, position);
                 CREATE TRIGGER links_work_item_delete AFTER DELETE ON work_items BEGIN
                     DELETE FROM links WHERE (src_type = 'work_item' AND src_id = OLD.id)
                                          OR (dst_type = 'work_item' AND dst_id = OLD.id);
                 END;
                 DROP TABLE suggestions;
                 DROP TABLE saved_views;
                 DROP TABLE project_timeline;
                 DROP TABLE pending_refetch;
                 DROP TABLE pending_rules;
                 DROP TABLE message_header_capture;
                 DROP TABLE message_headers;
                 DROP TABLE header_names;
                 DELETE FROM _migrations WHERE id IN (20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57);",
            )
            .unwrap();
        }
        let db = Database::open(&path).expect("upgrade reopen");
        assert_eq!(db.schema_version().unwrap(), latest_schema_version());
        let conn = db.lock_conn();
        let has_column: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('operations')
                 WHERE name = 'not_before_epoch'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_column, 1, "0020 re-adds the column on upgrade");
        drop(conn);
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
    }

    #[test]
    fn pre_0031_profile_upgrades_cleanly_and_regains_message_embeddings() {
        // A v30 profile in the wild: drop 0031's table + watermark, then
        // reopen. The upgrade must re-create `message_embeddings` so a later
        // `embed_message` job has somewhere to store its vector.
        //
        // MUST also un-apply every migration ABOVE 31 (here, just 32) even
        // though this test targets 31 specifically: the runner advances
        // past migration N only when N > MAX(id) already in `_migrations`
        // (see `run_migrations`), so leaving a newer id's row in place
        // watermarks past 31 and it silently never re-applies — the exact
        // "stray newer row skips older migrations" trap this pins against.
        let path = tmp_db();
        {
            let db = Database::open(&path).expect("first open");
            let conn = db.lock_conn();
            conn.execute_batch(
                "DROP TABLE message_embeddings;
                 DROP TABLE calendar_invites;
                 DROP TABLE thread_extractions;
                 DROP TABLE saved_filters;
                 DROP TABLE tag_definitions;
                 DROP TABLE message_keywords;
                 DROP INDEX idx_messages_account_rfc822;
                 DROP TABLE work_item_provenance;
                 DROP TABLE notification_outbox;
                 DROP TABLE vip_senders;
                 DROP TABLE thread_mutes;
                 ALTER TABLE threads DROP COLUMN is_muted;
                 ALTER TABLE drafts DROP COLUMN server_mailbox_path;
                 ALTER TABLE drafts DROP COLUMN server_uid;
                 ALTER TABLE drafts DROP COLUMN server_uid_validity;
                 ALTER TABLE drafts DROP COLUMN server_message_id;
                 ALTER TABLE drafts DROP COLUMN server_saved_at_epoch;
                 DROP INDEX idx_message_addresses_lower_address;
                 DROP TABLE thread_rebuild_state;
                 DROP TABLE thread_dirty_messages;
                 DROP TRIGGER messages_before_delete_fts;
                 DROP TABLE message_fts_rowid;
                 DROP TABLE message_fts;
                 CREATE VIRTUAL TABLE message_fts USING fts5(
                     message_id UNINDEXED, subject, sender, body, tokenize = 'unicode61');
                 CREATE TRIGGER messages_after_delete_fts AFTER DELETE ON messages BEGIN
                     DELETE FROM message_fts WHERE message_id = OLD.id;
                 END;
                 ALTER TABLE sync_cursors DROP COLUMN descriptor_checkpoint_uid;
                 ALTER TABLE smart_mailboxes DROP COLUMN group_json;
                 ALTER TABLE smart_mailboxes DROP COLUMN scope_json;
                 DROP TABLE message_decode_skips;
                 ALTER TABLE account_identities DROP COLUMN reply_to;
                 ALTER TABLE account_identities DROP COLUMN bcc;
                 ALTER TABLE account_identities DROP COLUMN is_default;
                 DROP TABLE work_items;
                 CREATE TABLE work_items (
                     id TEXT PRIMARY KEY,
                     project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                     title TEXT NOT NULL,
                     notes TEXT NOT NULL DEFAULT '',
                     state TEXT NOT NULL DEFAULT 'needs_reply'
                         CHECK (state IN ('needs_reply','waiting','delegated','blocked','done','someday')),
                     position INTEGER NOT NULL DEFAULT 0,
                     created_at INTEGER NOT NULL,
                     updated_at INTEGER NOT NULL
                 );
                 CREATE INDEX idx_work_items_project ON work_items(project_id, state, position);
                 CREATE TRIGGER links_work_item_delete AFTER DELETE ON work_items BEGIN
                     DELETE FROM links WHERE (src_type = 'work_item' AND src_id = OLD.id)
                                          OR (dst_type = 'work_item' AND dst_id = OLD.id);
                 END;
                 DROP TABLE suggestions;
                 DROP TABLE saved_views;
                 DROP TABLE project_timeline;
                 DROP TABLE pending_refetch;
                 DROP TABLE pending_rules;
                 DROP TABLE message_header_capture;
                 DROP TABLE message_headers;
                 DROP TABLE header_names;
                 DELETE FROM _migrations WHERE id IN (31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57);",
            )
            .unwrap();
        }
        let db = Database::open(&path).expect("upgrade reopen");
        assert_eq!(db.schema_version().unwrap(), latest_schema_version());
        let conn = db.lock_conn();
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table'
                 AND name='message_embeddings'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "0031 re-creates message_embeddings on upgrade");
        drop(conn);
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
    }

    #[test]
    fn pre_0032_profile_upgrades_cleanly_and_regains_calendar_invites() {
        // A v31 profile in the wild: drop 0032's table + watermark, then
        // reopen. The upgrade must re-create `calendar_invites` so a later
        // hydrated invite has somewhere to record the user's response. 0033
        // goes too: leaving it would sit above the MAX(id) watermark and the
        // runner would step over 0032 entirely.
        let path = tmp_db();
        {
            let db = Database::open(&path).expect("first open");
            let conn = db.lock_conn();
            conn.execute_batch(
                "DROP TABLE calendar_invites;
                 DROP TABLE thread_extractions;
                 DROP TABLE saved_filters;
                 DROP TABLE tag_definitions;
                 DROP TABLE message_keywords;
                 DROP INDEX idx_messages_account_rfc822;
                 DROP TABLE work_item_provenance;
                 DROP TABLE notification_outbox;
                 DROP TABLE vip_senders;
                 DROP TABLE thread_mutes;
                 ALTER TABLE threads DROP COLUMN is_muted;
                 ALTER TABLE drafts DROP COLUMN server_mailbox_path;
                 ALTER TABLE drafts DROP COLUMN server_uid;
                 ALTER TABLE drafts DROP COLUMN server_uid_validity;
                 ALTER TABLE drafts DROP COLUMN server_message_id;
                 ALTER TABLE drafts DROP COLUMN server_saved_at_epoch;
                 DROP INDEX idx_message_addresses_lower_address;
                 DROP TABLE thread_rebuild_state;
                 DROP TABLE thread_dirty_messages;
                 DROP TRIGGER messages_before_delete_fts;
                 DROP TABLE message_fts_rowid;
                 DROP TABLE message_fts;
                 CREATE VIRTUAL TABLE message_fts USING fts5(
                     message_id UNINDEXED, subject, sender, body, tokenize = 'unicode61');
                 CREATE TRIGGER messages_after_delete_fts AFTER DELETE ON messages BEGIN
                     DELETE FROM message_fts WHERE message_id = OLD.id;
                 END;
                 ALTER TABLE sync_cursors DROP COLUMN descriptor_checkpoint_uid;
                 ALTER TABLE smart_mailboxes DROP COLUMN group_json;
                 ALTER TABLE smart_mailboxes DROP COLUMN scope_json;
                 DROP TABLE message_decode_skips;
                 ALTER TABLE account_identities DROP COLUMN reply_to;
                 ALTER TABLE account_identities DROP COLUMN bcc;
                 ALTER TABLE account_identities DROP COLUMN is_default;
                 DROP TABLE work_items;
                 CREATE TABLE work_items (
                     id TEXT PRIMARY KEY,
                     project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                     title TEXT NOT NULL,
                     notes TEXT NOT NULL DEFAULT '',
                     state TEXT NOT NULL DEFAULT 'needs_reply'
                         CHECK (state IN ('needs_reply','waiting','delegated','blocked','done','someday')),
                     position INTEGER NOT NULL DEFAULT 0,
                     created_at INTEGER NOT NULL,
                     updated_at INTEGER NOT NULL
                 );
                 CREATE INDEX idx_work_items_project ON work_items(project_id, state, position);
                 CREATE TRIGGER links_work_item_delete AFTER DELETE ON work_items BEGIN
                     DELETE FROM links WHERE (src_type = 'work_item' AND src_id = OLD.id)
                                          OR (dst_type = 'work_item' AND dst_id = OLD.id);
                 END;
                 DROP TABLE suggestions;
                 DROP TABLE saved_views;
                 DROP TABLE project_timeline;
                 DROP TABLE pending_refetch;
                 DROP TABLE pending_rules;
                 DROP TABLE message_header_capture;
                 DROP TABLE message_headers;
                 DROP TABLE header_names;
                 DELETE FROM _migrations WHERE id IN (32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57);",
            )
            .unwrap();
        }
        let db = Database::open(&path).expect("upgrade reopen");
        assert_eq!(db.schema_version().unwrap(), latest_schema_version());
        let conn = db.lock_conn();
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table'
                 AND name='calendar_invites'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "0032 re-creates calendar_invites on upgrade");
        drop(conn);
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
    }

    #[test]
    fn pre_0033_profile_upgrades_cleanly_and_regains_thread_extractions() {
        // A v32 profile in the wild: drop 0033's table + watermark (and 0034
        // above it, per the "un-apply every migration above N" trap the
        // pre_0031/pre_0032 tests already pin), then reopen. The upgrade must
        // re-create `thread_extractions` so a later `extract_thread_items`
        // job has somewhere to store its rows.
        let path = tmp_db();
        {
            let db = Database::open(&path).expect("first open");
            let conn = db.lock_conn();
            conn.execute_batch(
                "DROP TABLE thread_extractions;
                 DROP TABLE saved_filters;
                 DROP TABLE tag_definitions;
                 DROP TABLE message_keywords;
                 DROP INDEX idx_messages_account_rfc822;
                 DROP TABLE work_item_provenance;
                 DROP TABLE notification_outbox;
                 DROP TABLE vip_senders;
                 DROP TABLE thread_mutes;
                 ALTER TABLE threads DROP COLUMN is_muted;
                 ALTER TABLE drafts DROP COLUMN server_mailbox_path;
                 ALTER TABLE drafts DROP COLUMN server_uid;
                 ALTER TABLE drafts DROP COLUMN server_uid_validity;
                 ALTER TABLE drafts DROP COLUMN server_message_id;
                 ALTER TABLE drafts DROP COLUMN server_saved_at_epoch;
                 DROP INDEX idx_message_addresses_lower_address;
                 DROP TABLE thread_rebuild_state;
                 DROP TABLE thread_dirty_messages;
                 DROP TRIGGER messages_before_delete_fts;
                 DROP TABLE message_fts_rowid;
                 DROP TABLE message_fts;
                 CREATE VIRTUAL TABLE message_fts USING fts5(
                     message_id UNINDEXED, subject, sender, body, tokenize = 'unicode61');
                 CREATE TRIGGER messages_after_delete_fts AFTER DELETE ON messages BEGIN
                     DELETE FROM message_fts WHERE message_id = OLD.id;
                 END;
                 ALTER TABLE sync_cursors DROP COLUMN descriptor_checkpoint_uid;
                 ALTER TABLE smart_mailboxes DROP COLUMN group_json;
                 ALTER TABLE smart_mailboxes DROP COLUMN scope_json;
                 DROP TABLE message_decode_skips;
                 ALTER TABLE account_identities DROP COLUMN reply_to;
                 ALTER TABLE account_identities DROP COLUMN bcc;
                 ALTER TABLE account_identities DROP COLUMN is_default;
                 DROP TABLE work_items;
                 CREATE TABLE work_items (
                     id TEXT PRIMARY KEY,
                     project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                     title TEXT NOT NULL,
                     notes TEXT NOT NULL DEFAULT '',
                     state TEXT NOT NULL DEFAULT 'needs_reply'
                         CHECK (state IN ('needs_reply','waiting','delegated','blocked','done','someday')),
                     position INTEGER NOT NULL DEFAULT 0,
                     created_at INTEGER NOT NULL,
                     updated_at INTEGER NOT NULL
                 );
                 CREATE INDEX idx_work_items_project ON work_items(project_id, state, position);
                 CREATE TRIGGER links_work_item_delete AFTER DELETE ON work_items BEGIN
                     DELETE FROM links WHERE (src_type = 'work_item' AND src_id = OLD.id)
                                          OR (dst_type = 'work_item' AND dst_id = OLD.id);
                 END;
                 DROP TABLE suggestions;
                 DROP TABLE saved_views;
                 DROP TABLE project_timeline;
                 ALTER TABLE calendar_invites DROP COLUMN location;
                 ALTER TABLE calendar_invites DROP COLUMN description;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_raw;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_tzid;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_is_utc;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_is_date_only;
                 DROP TABLE pending_refetch;
                 DROP TABLE pending_rules;
                 DROP TABLE message_header_capture;
                 DROP TABLE message_headers;
                 DROP TABLE header_names;
                 DELETE FROM _migrations WHERE id IN (33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57);",
            )
            .unwrap();
        }
        let db = Database::open(&path).expect("upgrade reopen");
        assert_eq!(db.schema_version().unwrap(), latest_schema_version());
        let conn = db.lock_conn();
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table'
                 AND name='thread_extractions'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "0033 re-creates thread_extractions on upgrade");
        drop(conn);
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
    }

    #[test]
    fn pre_0034_profile_upgrades_cleanly_and_regains_saved_filters() {
        // A v33 profile in the wild: drop 0034's table + watermark only, then
        // reopen. The upgrade must re-create `saved_filters` so a later rule
        // or smart mailbox has somewhere to be stored. id 34 is the sole gap,
        // so nothing higher is left in `_migrations` to falsely satisfy the
        // MAX(id) watermark check in `run_migrations`.
        let path = tmp_db();
        {
            let db = Database::open(&path).expect("first open");
            let conn = db.lock_conn();
            conn.execute_batch(
                "DROP TABLE saved_filters;
                 DROP TABLE tag_definitions;
                 DROP TABLE message_keywords;
                 DROP INDEX idx_messages_account_rfc822;
                 DROP TABLE work_item_provenance;
                 DROP TABLE notification_outbox;
                 DROP TABLE vip_senders;
                 DROP TABLE thread_mutes;
                 ALTER TABLE threads DROP COLUMN is_muted;
                 ALTER TABLE drafts DROP COLUMN server_mailbox_path;
                 ALTER TABLE drafts DROP COLUMN server_uid;
                 ALTER TABLE drafts DROP COLUMN server_uid_validity;
                 ALTER TABLE drafts DROP COLUMN server_message_id;
                 ALTER TABLE drafts DROP COLUMN server_saved_at_epoch;
                 DROP INDEX idx_message_addresses_lower_address;
                 DROP TABLE thread_rebuild_state;
                 DROP TABLE thread_dirty_messages;
                 DROP TRIGGER messages_before_delete_fts;
                 DROP TABLE message_fts_rowid;
                 DROP TABLE message_fts;
                 CREATE VIRTUAL TABLE message_fts USING fts5(
                     message_id UNINDEXED, subject, sender, body, tokenize = 'unicode61');
                 CREATE TRIGGER messages_after_delete_fts AFTER DELETE ON messages BEGIN
                     DELETE FROM message_fts WHERE message_id = OLD.id;
                 END;
                 ALTER TABLE sync_cursors DROP COLUMN descriptor_checkpoint_uid;
                 ALTER TABLE smart_mailboxes DROP COLUMN group_json;
                 ALTER TABLE smart_mailboxes DROP COLUMN scope_json;
                 DROP TABLE message_decode_skips;
                 ALTER TABLE account_identities DROP COLUMN reply_to;
                 ALTER TABLE account_identities DROP COLUMN bcc;
                 ALTER TABLE account_identities DROP COLUMN is_default;
                 DROP TABLE work_items;
                 CREATE TABLE work_items (
                     id TEXT PRIMARY KEY,
                     project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                     title TEXT NOT NULL,
                     notes TEXT NOT NULL DEFAULT '',
                     state TEXT NOT NULL DEFAULT 'needs_reply'
                         CHECK (state IN ('needs_reply','waiting','delegated','blocked','done','someday')),
                     position INTEGER NOT NULL DEFAULT 0,
                     created_at INTEGER NOT NULL,
                     updated_at INTEGER NOT NULL
                 );
                 CREATE INDEX idx_work_items_project ON work_items(project_id, state, position);
                 CREATE TRIGGER links_work_item_delete AFTER DELETE ON work_items BEGIN
                     DELETE FROM links WHERE (src_type = 'work_item' AND src_id = OLD.id)
                                          OR (dst_type = 'work_item' AND dst_id = OLD.id);
                 END;
                 DROP TABLE suggestions;
                 DROP TABLE saved_views;
                 DROP TABLE project_timeline;
                 ALTER TABLE calendar_invites DROP COLUMN location;
                 ALTER TABLE calendar_invites DROP COLUMN description;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_raw;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_tzid;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_is_utc;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_is_date_only;
                 DROP TABLE pending_refetch;
                 DROP TABLE pending_rules;
                 DROP TABLE message_header_capture;
                 DROP TABLE message_headers;
                 DROP TABLE header_names;
                 DELETE FROM _migrations WHERE id IN (34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57);",
            )
            .unwrap();
        }
        let db = Database::open(&path).expect("upgrade reopen");
        assert_eq!(db.schema_version().unwrap(), latest_schema_version());
        let conn = db.lock_conn();
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table'
                 AND name='saved_filters'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "0034 re-creates saved_filters on upgrade");
        drop(conn);
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
    }

    #[test]
    fn pre_0039_profile_carries_a_pending_dirty_marker_into_force_full() {
        // A v38 profile whose 0037 boolean says the projection is stale has
        // no per-message rows to say WHAT is stale, so 0039 must carry the
        // marker over as force_full -- otherwise the incremental rebuild
        // reads "nothing dirty" and the pending rebuild never happens (PR
        // #181 follow-up). 40 is reverted too: left applied, the MAX(id)
        // watermark would read "past 39" and 0039 would never re-run.
        let path = tmp_db();
        {
            let db = Database::open(&path).expect("first open");
            let conn = db.lock_conn();
            conn.execute_batch(
                "DROP TABLE thread_dirty_messages;
                 ALTER TABLE thread_rebuild_state DROP COLUMN force_full;
                 INSERT INTO accounts (id, email, provider) VALUES ('stale', 's@x.test', 'imap');
                 INSERT INTO accounts (id, email, provider) VALUES ('clean', 'c@x.test', 'imap');
                 INSERT INTO thread_rebuild_state (account_id, dirty) VALUES ('stale', 1);
                 INSERT INTO thread_rebuild_state (account_id, dirty) VALUES ('clean', 0);
                 ALTER TABLE sync_cursors DROP COLUMN descriptor_checkpoint_uid;
                 ALTER TABLE smart_mailboxes DROP COLUMN group_json;
                 ALTER TABLE smart_mailboxes DROP COLUMN scope_json;
                 DROP TABLE message_decode_skips;
                 ALTER TABLE saved_filters DROP COLUMN sort_order;
                 ALTER TABLE saved_filters DROP COLUMN stop_processing;
                 ALTER TABLE saved_filters DROP COLUMN enabled;
                 ALTER TABLE account_identities DROP COLUMN reply_to;
                 ALTER TABLE account_identities DROP COLUMN bcc;
                 ALTER TABLE account_identities DROP COLUMN is_default;
                 DROP TABLE tag_definitions;
                 DROP TABLE message_keywords;
                 DROP INDEX idx_messages_account_rfc822;
                 DROP TABLE work_item_provenance;
                 DROP TABLE notification_outbox;
                 DROP TABLE vip_senders;
                 DROP TABLE thread_mutes;
                 ALTER TABLE threads DROP COLUMN is_muted;
                 DROP TABLE work_items;
                 CREATE TABLE work_items (
                     id TEXT PRIMARY KEY,
                     project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                     title TEXT NOT NULL,
                     notes TEXT NOT NULL DEFAULT '',
                     state TEXT NOT NULL DEFAULT 'needs_reply'
                         CHECK (state IN ('needs_reply','waiting','delegated','blocked','done','someday')),
                     position INTEGER NOT NULL DEFAULT 0,
                     created_at INTEGER NOT NULL,
                     updated_at INTEGER NOT NULL
                 );
                 CREATE INDEX idx_work_items_project ON work_items(project_id, state, position);
                 CREATE TRIGGER links_work_item_delete AFTER DELETE ON work_items BEGIN
                     DELETE FROM links WHERE (src_type = 'work_item' AND src_id = OLD.id)
                                          OR (dst_type = 'work_item' AND dst_id = OLD.id);
                 END;
                 DROP TABLE suggestions;
                 DROP TABLE saved_views;
                 DROP TABLE project_timeline;
                 ALTER TABLE calendar_invites DROP COLUMN location;
                 ALTER TABLE calendar_invites DROP COLUMN description;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_raw;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_tzid;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_is_utc;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_is_date_only;
                 DROP TABLE pending_refetch;
                 DROP TABLE pending_rules;
                 DROP TABLE message_header_capture;
                 DROP TABLE message_headers;
                 DROP TABLE header_names;
                 DELETE FROM _migrations WHERE id IN (39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57);",
            )
            .unwrap();
        }
        let db = Database::open(&path).expect("upgrade reopen");
        assert_eq!(db.schema_version().unwrap(), latest_schema_version());
        let force_full = |account: &str| -> i64 {
            db.lock_conn()
                .query_row(
                    "SELECT force_full FROM thread_rebuild_state WHERE account_id = ?1",
                    [account],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(force_full("stale"), 1);
        assert_eq!(force_full("clean"), 0);
        assert_eq!(
            db.rebuild_threads_incremental("stale").unwrap(),
            ThreadRebuildPath::Full(FullThreadRebuildReason::ForceFull)
        );
        assert!(!db.threads_dirty("stale").unwrap());
        drop(db);
        remove_db_and_sidecars(&path);
    }

    /// `Database` has no `Debug` impl (see its doc comment), so a
    /// `Result<Database, OpenError>` can't use `expect_err`/`{:?}` directly;
    /// this renders just enough to name a test failure.
    fn open_result_debug(r: &Result<Database, OpenError>) -> String {
        match r {
            Ok(_) => "Ok(<opened database>)".to_string(),
            Err(e) => format!("Err({e:?})"),
        }
    }

    /// Lists `<db>.pre-*.bak` backup files next to `db_path`, for asserting
    /// backup-creation and retention behaviour without hardcoding ids.
    fn backup_files(db_path: &std::path::Path) -> Vec<std::path::PathBuf> {
        let dir = db_path.parent().unwrap();
        let stem = db_path.file_name().unwrap().to_str().unwrap();
        let prefix = format!("{stem}.pre-");
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.starts_with(&prefix) && name.ends_with(".bak")
            })
            .map(|e| e.path())
            .collect()
    }

    fn remove_db_and_sidecars(path: &std::path::Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
        for b in backup_files(path) {
            let _ = std::fs::remove_file(b);
        }
    }

    /// Pins the exact prefix text `CastellumLaunchError` (Swift) matches —
    /// see the doc comments on `SCHEMA_TOO_NEW_PREFIX`/`MIGRATION_FAILED_PREFIX`.
    /// Rewording either without updating the Swift classifier in the same
    /// change breaks the launch-error screen silently (no compiler error
    /// crosses the FFI boundary).
    #[test]
    fn open_error_prefixes_stay_stable_for_the_swift_classifier() {
        assert_eq!(SCHEMA_TOO_NEW_PREFIX, "schema too new: ");
        assert_eq!(MIGRATION_FAILED_PREFIX, "migration failed: ");
    }

    /// (1) An older build must refuse a profile a newer build already
    /// migrated further, before writing anything.
    #[test]
    fn open_refuses_a_schema_stamped_newer_than_this_build_and_leaves_bytes_unchanged() {
        let path = tmp_db();
        {
            let db = Database::open(&path).expect("first open migrates to latest");
            // Stamp a migration id no shipped build has ever produced, to
            // simulate an older binary opening a profile a newer one
            // already migrated past what this build's MIGRATIONS knows.
            let conn = db.lock_conn();
            conn.execute(
                "INSERT INTO _migrations (id, name) VALUES (?1, 'from_the_future')",
                rusqlite::params![latest_schema_version() + 1],
            )
            .unwrap();
        }
        let before = std::fs::read(&path).expect("read db bytes before refused reopen");
        match Database::open(&path) {
            Err(OpenError::SchemaTooNew { found, supported }) => {
                assert_eq!(found, latest_schema_version() + 1);
                assert_eq!(supported, latest_schema_version());
            }
            other => panic!(
                "expected Err(SchemaTooNew), got {}",
                open_result_debug(&other)
            ),
        }
        let after = std::fs::read(&path).expect("read db bytes after refused reopen");
        assert_eq!(
            before, after,
            "a refused open must not write anything to the profile"
        );
        remove_db_and_sidecars(&path);
    }

    /// (2) A migration that fails partway rolls back cleanly, leaves the
    /// prior watermark, names itself in `MigrationFailed`, and a
    /// pre-migration backup exists and itself opens (proving it's a valid,
    /// consistent snapshot, not a half-written file).
    #[test]
    fn a_failing_migration_rolls_back_backs_up_first_and_names_itself() {
        let path = tmp_db();
        let before_version = {
            let db = Database::open(&path).expect("first open");
            // Seed a row so this profile is unambiguously non-empty (a
            // brand-new profile gets no backup — see test 3 below) and so
            // rollback-preserves-data has something concrete to check.
            {
                let conn = db.lock_conn();
                conn.execute(
                    "INSERT INTO app_meta (key, value) VALUES ('probe', 'v1')",
                    [],
                )
                .unwrap();
                // `conn` must drop before `schema_version()` below, which
                // locks the same (non-reentrant) mutex itself.
            }
            db.schema_version().unwrap()
        };

        let db = Database::open(&path).expect("reopen for the injected failure");
        // Deliberately DOES real, valid DDL/DML before the fatal statement —
        // `execute_batch` runs each statement in turn, so a migration that
        // fails at prepare on its FIRST statement (e.g. plain garbage SQL)
        // never actually writes anything, which would let this test pass
        // even with no rollback at all. `rb_probe` must exist mid-batch and
        // must NOT exist afterward for this test to mean anything.
        let broken = [Migration {
            id: latest_schema_version() + 1,
            name: "broken_test_migration",
            sql: "CREATE TABLE rb_probe(x); INSERT INTO rb_probe VALUES(1); NOT SQL;",
        }];
        let err = db
            .run_migration_set(&broken, latest_schema_version() + 1, &path)
            .expect_err("a broken migration must fail");
        let backup_path = match err {
            OpenError::MigrationFailed {
                id,
                name,
                backup_path,
                ..
            } => {
                assert_eq!(id, latest_schema_version() + 1);
                assert_eq!(name, "broken_test_migration");
                backup_path.expect("a non-empty profile must get a pre-migration backup")
            }
            other => panic!("expected MigrationFailed, got {other:?}"),
        };

        assert!(
            std::path::Path::new(&backup_path).exists(),
            "the backup file named in the error must actually exist"
        );
        // The backup must be a valid, consistent, openable snapshot — not a
        // half-written file — taken BEFORE the failed migration.
        let backup_db =
            Database::open(std::path::Path::new(&backup_path)).expect("backup must open");
        assert_eq!(backup_db.schema_version().unwrap(), before_version);
        {
            let conn = backup_db.lock_conn();
            let probe: String = conn
                .query_row("SELECT value FROM app_meta WHERE key='probe'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(probe, "v1", "the backup carries the seeded row");
        }
        drop(backup_db);
        remove_db_and_sidecars(std::path::Path::new(&backup_path));

        // The live profile itself: watermark unchanged, seeded row intact —
        // the rollback left no trace of the attempted migration.
        assert_eq!(
            db.schema_version().unwrap(),
            before_version,
            "watermark must be unchanged after a rolled-back migration"
        );
        {
            let conn = db.lock_conn();
            let probe: String = conn
                .query_row("SELECT value FROM app_meta WHERE key='probe'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(probe, "v1");
            let rb_probe_exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='rb_probe'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                rb_probe_exists, 0,
                "rb_probe was created mid-batch by the failed migration; a real rollback must \
                 undo it, not just leave the watermark alone"
            );
        }
        drop(db);
        remove_db_and_sidecars(&path);
    }

    /// A commit-time failure — not a prepare/execute failure — must still
    /// classify as `MigrationFailed`, not fall through to a generic
    /// `OpenError::Io` (which `CoreError::classify`/`CastellumLaunchError`
    /// on the Swift side doesn't recognise, so `AppModel` would retry and
    /// hit its unconditional `fatalError` instead of showing the recovery
    /// alert). On a full disk in WAL mode this is the REALISTIC failure
    /// point (the FS runs out of room writing WAL frames at COMMIT, not
    /// earlier), so this exercises that path deterministically: `PRAGMA
    /// defer_foreign_keys=ON` postpones a foreign-key violation's check
    /// from each statement to the transaction's COMMIT, so
    /// `tx.execute_batch` succeeds and only `tx.commit()` fails.
    #[test]
    fn a_commit_time_failure_returns_migration_failed_not_io() {
        let path = tmp_db();
        let db = Database::open(&path).expect("first open");
        let deferred_fk_violation = [Migration {
            id: latest_schema_version() + 1,
            name: "deferred_fk_violation_at_commit",
            sql: "PRAGMA defer_foreign_keys=ON;\n\
                  CREATE TABLE cft_parent(id INTEGER PRIMARY KEY);\n\
                  CREATE TABLE cft_child(\n\
                      id INTEGER PRIMARY KEY,\n\
                      parent_id INTEGER NOT NULL REFERENCES cft_parent(id)\n\
                  );\n\
                  INSERT INTO cft_child (id, parent_id) VALUES (1, 999);",
        }];
        let err = db
            .run_migration_set(&deferred_fk_violation, latest_schema_version() + 1, &path)
            .expect_err("a deferred FK violation must fail at COMMIT");
        match err {
            OpenError::MigrationFailed { id, name, .. } => {
                assert_eq!(id, latest_schema_version() + 1);
                assert_eq!(name, "deferred_fk_violation_at_commit");
            }
            other => panic!(
                "expected MigrationFailed for a commit-time failure (not Io), got {}",
                open_result_debug(&Err(other))
            ),
        }
        assert_eq!(
            db.schema_version().unwrap(),
            latest_schema_version(),
            "watermark must be unchanged — the whole transaction, DDL included, rolled back"
        );
        {
            let conn = db.lock_conn();
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='cft_parent'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                exists, 0,
                "the failed migration's DDL must have rolled back along with its data"
            );
        }
        drop(db);
        remove_db_and_sidecars(&path);
    }

    /// A `VACUUM INTO` that fails partway — simulated here with a real
    /// unwritable target directory, per the review's exact scenario, NOT
    /// the shared system tmp dir (this test gets its own isolated
    /// subdirectory: chmod-ing the shared tmp dir, even briefly, would make
    /// a concurrently-running test's unrelated file creation fail too) —
    /// must leave no partial `.bak.partial`/`-journal` behind, AND must not
    /// block the migration itself — the backup is best-effort. Directory
    /// permissions gate CREATING a new entry, not writing through an
    /// already-open fd to an existing one, so a plain commit on the SAME
    /// already-open connection is unaffected — only `VACUUM INTO`'s
    /// brand-new target file creation fails. Deliberately keeps ONE
    /// connection open across the whole test rather than closing and
    /// reopening after the chmod: SQLite may need to recreate a WAL/`-shm`
    /// sidecar on `open()`, which — empirically confirmed while writing
    /// this test — makes it report the WHOLE database read-only once its
    /// directory is unwritable, defeating the point (this test wants ONLY
    /// the backup to fail, not the migration).
    #[test]
    fn a_failed_backup_leaves_no_partial_artifacts_and_the_migration_still_runs() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let isolated_dir = castellum_testutil::scratch_root().join(format!(
            "castellum-db-backup-fail-test-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&isolated_dir).expect("create isolated test dir");
        let path = isolated_dir.join("castellum.db");

        let db = Database::open(&path).expect("first open");
        let before_version = db.schema_version().unwrap();

        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&isolated_dir, std::fs::Permissions::from_mode(0o500))
            .expect("make the isolated dir unwritable");

        // Root (CAP_DAC_OVERRIDE) ignores the mode bits, so the backup would
        // succeed and this test would assert against a scenario that never
        // happened. Probe by creating an entry; if that works, skip.
        let probe = isolated_dir.join("writability.probe");
        if std::fs::File::create(&probe).is_ok() {
            let _ = std::fs::remove_file(&probe);
            let _ = std::fs::set_permissions(&isolated_dir, std::fs::Permissions::from_mode(0o700));
            drop(db);
            remove_db_and_sidecars(&path);
            let _ = std::fs::remove_dir_all(&isolated_dir);
            eprintln!(
                "skipping a_failed_backup_leaves_no_partial_artifacts_and_the_migration_still_runs: \
                 a 0o500 directory is still writable here (running as root?)"
            );
            return;
        }

        let next_id = latest_schema_version() + 1;
        let extra = [Migration {
            id: next_id,
            name: "backup_blocked_probe",
            sql: "SELECT 1;",
        }];
        let outcome = db.run_migration_set(&extra, next_id, &path);

        // Restore writability before any assertion/cleanup can run, whatever
        // happened above — otherwise this leaks an undeletable directory.
        std::fs::set_permissions(&isolated_dir, std::fs::Permissions::from_mode(0o700))
            .expect("restore the isolated dir's permissions");

        outcome.expect("the migration must still succeed even though its backup failed");
        let final_version = db.schema_version().unwrap();
        assert!(
            final_version > before_version,
            "sanity: this test must actually exercise a pending migration"
        );
        assert!(
            backup_files(&path).is_empty(),
            "a failed VACUUM INTO must leave no complete .bak file"
        );
        let (dir, stem) = Database::backup_dir_and_stem(&path);
        let would_be_partial = dir.join(format!("{stem}.pre-{final_version}.bak.partial"));
        assert!(
            !would_be_partial.exists(),
            "no .bak.partial must survive a failed backup attempt"
        );
        assert!(
            !journal_sidecar(&would_be_partial).exists(),
            "no -journal sidecar must survive a failed backup attempt"
        );

        drop(db);
        remove_db_and_sidecars(&path);
        let _ = std::fs::remove_dir_all(&isolated_dir);
    }

    /// The free-space precheck must skip the backup — not the migration —
    /// when the profile volume doesn't have comfortable headroom, using the
    /// `FREE_SPACE_OVERRIDE_FOR_TEST` seam rather than actually filling a
    /// disk.
    #[test]
    fn the_free_space_precheck_skips_the_backup_not_the_migration() {
        let path = tmp_db();
        let before_version = {
            let db = Database::open(&path).expect("first open");
            db.schema_version().unwrap()
        };
        FREE_SPACE_OVERRIDE_FOR_TEST.with(|c| c.set(Some(1024))); // 1KB: never enough
        let next_id = latest_schema_version().max(before_version) + 1;
        let outcome: Result<i64, OpenError> = (|| {
            let db = Database::open(&path)?;
            let extra = [Migration {
                id: next_id,
                name: "low_free_space_probe",
                sql: "SELECT 1;",
            }];
            db.run_migration_set(&extra, next_id, &path)?;
            Ok(db.schema_version().unwrap())
        })();
        // Reset the override before any assertion can fail this test, so a
        // failure here doesn't leak a forced "always low" reading into
        // whatever test runs next on this thread.
        FREE_SPACE_OVERRIDE_FOR_TEST.with(|c| c.set(None));
        let applied_version =
            outcome.expect("the migration must still succeed when only the backup is skipped");
        assert_eq!(applied_version, next_id);

        assert!(
            backup_files(&path).is_empty(),
            "the precheck must have skipped the backup entirely — no .bak file"
        );
        remove_db_and_sidecars(&path);
    }

    /// PR #178 review hardening: a migration that rewrites a large table
    /// (0038's `message_fts` rebuild) can leave hundreds of MB sitting in
    /// the WAL file until the app quits, since SQLite's own auto-checkpoint
    /// is opportunistic, not immediate. `open` must checkpoint and truncate
    /// it before returning.
    #[test]
    fn open_truncates_the_wal_file_after_migrations_run() {
        let path = tmp_db();
        let db = Database::open(&path).expect("open");
        // Dirty the WAL directly: many small writes, each its own
        // transaction (so they land as many WAL frames, like real usage),
        // past SQLite's default ~1000-page auto-checkpoint threshold, with
        // that auto-checkpoint disabled so this test isolates the explicit
        // TRUNCATE call from SQLite's own opportunistic one.
        {
            let conn = db.lock_conn();
            conn.execute_batch("PRAGMA wal_autocheckpoint=0;").unwrap();
            for i in 0..2000 {
                conn.execute(
                    "INSERT INTO accounts (id, email, display_name, provider) \
                     VALUES (?1, ?2, NULL, 'imap')",
                    rusqlite::params![format!("wal-probe-{i}"), format!("probe{i}@x.test")],
                )
                .unwrap();
            }
        }
        let wal_path = {
            let mut s = path.as_os_str().to_os_string();
            s.push("-wal");
            std::path::PathBuf::from(s)
        };
        let dirty_size = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
        assert!(
            dirty_size > 100_000,
            "the WAL must actually have grown for this test to mean anything: {dirty_size} bytes"
        );

        db.checkpoint_wal_truncate();

        let after_size = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
        assert!(
            after_size < dirty_size / 10,
            "wal_checkpoint(TRUNCATE) must shrink the WAL file back down: \
             {dirty_size} -> {after_size}"
        );

        drop(db);
        remove_db_and_sidecars(&path);
    }

    /// PR #178 review hardening: migration 0038 rebuilds `message_fts` from
    /// scratch (old + new copy coexist mid-migration), so it can need far
    /// more headroom than the backup's own 1.2x precheck leaves free
    /// afterward (~0.2x(DB) — see `take_pre_migration_backup`'s doc comment).
    /// A profile with a pending HEAVY migration must be refused BEFORE
    /// taking any backup or touching any data, with a typed, user-legible
    /// reason, rather than starting and then hitting SQLITE_FULL partway.
    #[test]
    fn a_heavy_migration_refuses_early_when_free_space_is_insufficient() {
        let path = tmp_db();
        {
            // A v37 profile: revert migrations 0038, 0039 and 0040's effects
            // in place (mirrors the `pre_0035_*` upgrade tests' pattern) so
            // this profile genuinely has 38 pending, not just a synthetic
            // probe. Every id >= 38 must go: the runner's watermark is
            // MAX(id), not "which ids are missing", so leaving 39 or 40 in
            // `_migrations` while only 38 is absent would read as "already
            // past 38" and this test would prove nothing (
            // review finding; `scripts/ci/check-migrations.py` rule 4).
            let db = Database::open(&path).expect("first open");
            let conn = db.lock_conn();
            conn.execute_batch(
                "DROP TRIGGER messages_before_delete_fts;
                 DROP TABLE message_fts_rowid;
                 DROP TABLE message_fts;
                 CREATE VIRTUAL TABLE message_fts USING fts5(
                     message_id UNINDEXED, subject, sender, body, tokenize = 'unicode61');
                 CREATE TRIGGER messages_after_delete_fts AFTER DELETE ON messages BEGIN
                     DELETE FROM message_fts WHERE message_id = OLD.id;
                 END;
                 DROP TABLE thread_dirty_messages;
                 ALTER TABLE thread_rebuild_state DROP COLUMN force_full;
                 ALTER TABLE sync_cursors DROP COLUMN descriptor_checkpoint_uid;
                 ALTER TABLE smart_mailboxes DROP COLUMN group_json;
                 ALTER TABLE smart_mailboxes DROP COLUMN scope_json;
                 DROP TABLE message_decode_skips;
                 ALTER TABLE saved_filters DROP COLUMN sort_order;
                 ALTER TABLE saved_filters DROP COLUMN stop_processing;
                 ALTER TABLE saved_filters DROP COLUMN enabled;
                 ALTER TABLE account_identities DROP COLUMN reply_to;
                 ALTER TABLE account_identities DROP COLUMN bcc;
                 ALTER TABLE account_identities DROP COLUMN is_default;
                 DROP TABLE tag_definitions;
                 DROP TABLE message_keywords;
                 DROP INDEX idx_messages_account_rfc822;
                 DROP TABLE work_item_provenance;
                 DROP TABLE notification_outbox;
                 DROP TABLE vip_senders;
                 DROP TABLE thread_mutes;
                 ALTER TABLE threads DROP COLUMN is_muted;
                 DROP TABLE work_items;
                 CREATE TABLE work_items (
                     id TEXT PRIMARY KEY,
                     project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                     title TEXT NOT NULL,
                     notes TEXT NOT NULL DEFAULT '',
                     state TEXT NOT NULL DEFAULT 'needs_reply'
                         CHECK (state IN ('needs_reply','waiting','delegated','blocked','done','someday')),
                     position INTEGER NOT NULL DEFAULT 0,
                     created_at INTEGER NOT NULL,
                     updated_at INTEGER NOT NULL
                 );
                 CREATE INDEX idx_work_items_project ON work_items(project_id, state, position);
                 CREATE TRIGGER links_work_item_delete AFTER DELETE ON work_items BEGIN
                     DELETE FROM links WHERE (src_type = 'work_item' AND src_id = OLD.id)
                                          OR (dst_type = 'work_item' AND dst_id = OLD.id);
                 END;
                 DROP TABLE suggestions;
                 DROP TABLE saved_views;
                 DROP TABLE project_timeline;
                 ALTER TABLE calendar_invites DROP COLUMN location;
                 ALTER TABLE calendar_invites DROP COLUMN description;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_raw;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_tzid;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_is_utc;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_is_date_only;
                 DROP TABLE pending_refetch;
                 DROP TABLE pending_rules;
                 DROP TABLE message_header_capture;
                 DROP TABLE message_headers;
                 DROP TABLE header_names;
                 DELETE FROM _migrations WHERE id IN (38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57);",
            )
            .unwrap();
        }

        // Below the flat 100MB floor alone, so this refuses regardless of
        // how small the test profile's own DB file is.
        FREE_SPACE_OVERRIDE_FOR_TEST.with(|c| c.set(Some(50 * 1024 * 1024)));
        let outcome = Database::open(&path);
        FREE_SPACE_OVERRIDE_FOR_TEST.with(|c| c.set(None));

        match outcome {
            Err(OpenError::MigrationFailed {
                id,
                name,
                reason,
                backup_path,
            }) => {
                assert_eq!(
                    id, 38,
                    "must name the actual heavy migration, not just the first pending id"
                );
                assert_eq!(name, "fts_rowid_index");
                assert!(
                    reason.to_lowercase().contains("free") && reason.contains("MB"),
                    "reason must tell the user how much space to free: {reason}"
                );
                assert!(
                    backup_path.is_none(),
                    "must refuse BEFORE attempting a backup, not after one that then fails"
                );
            }
            Err(other) => {
                panic!("expected MigrationFailed for insufficient free space, got {other:?}")
            }
            Ok(_) => {
                panic!("expected the open to refuse for insufficient free space, but it succeeded")
            }
        }
        assert!(
            backup_files(&path).is_empty(),
            "the early refusal must never have created a backup"
        );
        {
            // Nothing was touched: the watermark is still exactly 37, not
            // just "less than 38" -- a raw connection, so this doesn't run
            // Database::open's own migration/checkpoint logic again.
            let raw = Connection::open(&path).unwrap();
            let watermark: i64 = raw
                .query_row("SELECT COALESCE(MAX(id), 0) FROM _migrations", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(
                watermark, 37,
                "a refused migration must not advance the watermark"
            );
        }

        // Without the override, the same profile upgrades cleanly.
        let db = Database::open(&path).expect("reopen without the override must succeed");
        assert_eq!(db.schema_version().unwrap(), latest_schema_version());
        drop(db);
        remove_db_and_sidecars(&path);
    }

    // MARK: - pending_migration_count (: the launch preflight's
    // cheap probe, backing the "Upgrading your mail database…" window)

    /// A profile that doesn't exist yet (first-ever launch): every
    /// migration this build knows is "pending" — `open` will apply them
    /// all from empty — and never a write, never a real `Connection::open`.
    #[test]
    fn pending_migration_count_reports_all_pending_for_a_profile_that_does_not_exist_yet() {
        let path = tmp_db();
        assert!(!path.exists());
        let info = Database::pending_migration_count(&path).expect("probe must not fail");
        assert_eq!(
            info.applied_version, 0,
            "nothing has ever been applied to a profile that doesn't exist yet"
        );
        assert_eq!(info.pending_count, MIGRATIONS.len() as i64);
        assert!(
            info.has_heavy,
            "0038 is in MIGRATIONS, so a from-scratch profile has a heavy migration pending"
        );
        assert!(!path.exists(), "the probe must never create the file");
    }

    /// A freshly migrated (up-to-date) profile has nothing pending.
    #[test]
    fn pending_migration_count_is_zero_for_an_up_to_date_profile() {
        let path = tmp_db();
        let db = Database::open(&path).expect("open");
        drop(db);
        let info = Database::pending_migration_count(&path).expect("probe must not fail");
        assert_eq!(info.applied_version, latest_schema_version());
        assert_eq!(info.pending_count, 0);
        assert!(!info.has_heavy);
        remove_db_and_sidecars(&path);
    }

    /// A profile stamped past this build's `MIGRATIONS` (a downgrade
    /// scenario) must report `SchemaTooNew` from the cheap probe too, the
    /// same as `Database::open` would — the Swift preflight must never show
    /// the migrating window (or attempt the real open) for a profile #169's
    /// refusal is going to reject outright.
    #[test]
    fn pending_migration_count_reports_schema_too_new_without_touching_the_profile() {
        let path = tmp_db();
        let future_id = {
            let db = Database::open(&path).expect("open");
            let conn = db.lock_conn();
            let future_id = latest_schema_version() + 1;
            conn.execute(
                "INSERT INTO _migrations (id, name) VALUES (?1, 'from_the_future')",
                rusqlite::params![future_id],
            )
            .unwrap();
            future_id
        };

        match Database::pending_migration_count(&path) {
            Err(OpenError::SchemaTooNew { found, supported }) => {
                assert_eq!(found, future_id);
                assert_eq!(supported, latest_schema_version());
            }
            other => panic!("expected SchemaTooNew, got {other:?}"),
        }
        remove_db_and_sidecars(&path);
    }

    /// A profile from before migration 0050 keeps its work items and gains an
    /// empty provenance table (no backfill: history is not invented).
    #[test]
    fn pre_0050_profile_gains_the_provenance_table_and_keeps_work_items() {
        let path = tmp_db();
        {
            let db = Database::open(&path).expect("first open");
            let conn = db.lock_conn();
            conn.execute_batch(
                "INSERT INTO work_items (id, title, notes, state, position, created_at, updated_at)
                     VALUES ('w1', 'Keep me', 'user notes', 'needs_reply', 0, 1, 1);
                 DROP INDEX idx_messages_account_rfc822;
                 DROP TABLE work_item_provenance;
                 ALTER TABLE calendar_invites DROP COLUMN location;
                 ALTER TABLE calendar_invites DROP COLUMN description;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_raw;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_tzid;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_is_utc;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_is_date_only;
                 DROP TABLE pending_refetch;
                 DROP TABLE pending_rules;
                 DROP TABLE message_header_capture;
                 DROP TABLE message_headers;
                 DROP TABLE header_names;
                 DELETE FROM _migrations WHERE id IN (50, 51, 52, 53, 54, 55, 56, 57);",
            )
            .unwrap();
        }
        let info = Database::pending_migration_count(&path).expect("probe");
        assert_eq!(info.pending_count, 8, "0050-0057 are pending");
        let db = Database::open(&path).expect("reopen applies 0050");
        assert_eq!(db.schema_version().unwrap(), latest_schema_version());
        let (notes, prov): (String, i64) = db
            .lock_conn()
            .query_row(
                "SELECT (SELECT notes FROM work_items WHERE id = 'w1'),
                        (SELECT COUNT(*) FROM work_item_provenance)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((notes.as_str(), prov), ("user notes", 0));
        drop(db);
        remove_db_and_sidecars(&path);
    }

    /// MM-1c: a profile from before migration 0057 upgrades cleanly, keeps its
    /// messages, gains empty header tables, and deleting a message cascades.
    #[test]
    fn pre_0057_profile_gains_the_header_tables_and_keeps_messages() {
        let path = tmp_db();
        {
            let db = Database::open(&path).expect("first open");
            let conn = db.lock_conn();
            conn.execute_batch(
                "DROP TABLE message_header_capture;
                 DROP TABLE message_headers;
                 DROP TABLE header_names;
                 DELETE FROM _migrations WHERE id IN (57);",
            )
            .unwrap();
        }
        let info = Database::pending_migration_count(&path).expect("probe");
        assert_eq!(info.pending_count, 1, "0057 is pending");
        let db = Database::open(&path).expect("reopen applies 0057");
        assert_eq!(db.schema_version().unwrap(), latest_schema_version());
        let counts: (i64, i64, i64) = db
            .lock_conn()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM header_names),
                        (SELECT COUNT(*) FROM message_headers),
                        (SELECT COUNT(*) FROM message_header_capture)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(counts, (0, 0, 0));
        drop(db);
        remove_db_and_sidecars(&path);
    }

    /// MM-1: a profile from before migration 0042 keeps its text-query smart
    /// mailboxes and gains the (NULL) group/scope columns.
    #[test]
    fn pre_0042_profile_gains_smart_mailbox_group_columns_and_keeps_rows() {
        let path = tmp_db();
        {
            let db = Database::open(&path).expect("first open");
            let conn = db.lock_conn();
            conn.execute_batch(
                "INSERT INTO smart_mailboxes (id, name, query, sort_order, created_at, updated_at)
                     VALUES ('s1', 'Unread', 'is:unread', 0, 1, 1);
                 ALTER TABLE smart_mailboxes DROP COLUMN group_json;
                 ALTER TABLE smart_mailboxes DROP COLUMN scope_json;
                 ALTER TABLE saved_filters DROP COLUMN sort_order;
                 ALTER TABLE saved_filters DROP COLUMN stop_processing;
                 ALTER TABLE saved_filters DROP COLUMN enabled;
                 ALTER TABLE account_identities DROP COLUMN reply_to;
                 ALTER TABLE account_identities DROP COLUMN bcc;
                 ALTER TABLE account_identities DROP COLUMN is_default;
                 DROP TABLE work_items;
                 CREATE TABLE work_items (
                     id TEXT PRIMARY KEY,
                     project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                     title TEXT NOT NULL,
                     notes TEXT NOT NULL DEFAULT '',
                     state TEXT NOT NULL DEFAULT 'needs_reply'
                         CHECK (state IN ('needs_reply','waiting','delegated','blocked','done','someday')),
                     position INTEGER NOT NULL DEFAULT 0,
                     created_at INTEGER NOT NULL,
                     updated_at INTEGER NOT NULL
                 );
                 CREATE INDEX idx_work_items_project ON work_items(project_id, state, position);
                 CREATE TRIGGER links_work_item_delete AFTER DELETE ON work_items BEGIN
                     DELETE FROM links WHERE (src_type = 'work_item' AND src_id = OLD.id)
                                          OR (dst_type = 'work_item' AND dst_id = OLD.id);
                 END;
                 DROP TABLE suggestions;
                 DROP TABLE saved_views;
                 DROP TABLE project_timeline;
                 DROP INDEX idx_messages_account_rfc822;
                 DROP TABLE work_item_provenance;
                 DROP TABLE notification_outbox;
                 DROP TABLE vip_senders;
                 ALTER TABLE calendar_invites DROP COLUMN location;
                 ALTER TABLE calendar_invites DROP COLUMN description;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_raw;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_tzid;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_is_utc;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_is_date_only;
                 DROP TABLE pending_refetch;
                 DROP TABLE pending_rules;
                 DROP TABLE message_header_capture;
                 DROP TABLE message_headers;
                 DROP TABLE header_names;
                 DELETE FROM _migrations WHERE id IN (42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57);",
            )
            .unwrap();
        }
        let db = Database::open(&path).expect("reopen applies 0042");
        let sm = db.get_smart_mailbox("s1").unwrap().unwrap();
        assert_eq!(sm.query, "is:unread");
        assert!(sm.group.is_none());
        assert_eq!(sm.scope, castellum_models::SmartMailboxScope::default());
        assert_eq!(db.schema_version().unwrap(), latest_schema_version());
        drop(db);
        remove_db_and_sidecars(&path);
    }

    /// Active Work: a profile from before migration 0049 keeps its projects
    /// and work items, gains an empty append-only `project_timeline`, and the
    /// new table then works (append, no update, cascade on project delete).
    #[test]
    fn pre_0049_profile_gains_the_project_timeline_and_keeps_rows() {
        let path = tmp_db();
        let pid = {
            let db = Database::open(&path).expect("first open");
            let p = db
                .upsert_project(crate::ProjectUpsert {
                    name: "Kept",
                    status: "active",
                    position: 0,
                    ..Default::default()
                })
                .unwrap();
            db.upsert_work_item(crate::WorkItemUpsert {
                project_id: Some(&p.id),
                title: "Kept item",
                state: "waiting",
                ..Default::default()
            })
            .unwrap();
            let conn = db.lock_conn();
            conn.execute_batch(
                "DROP TABLE project_timeline;
                 DROP INDEX idx_messages_account_rfc822;
                 DROP TABLE work_item_provenance;
                 ALTER TABLE calendar_invites DROP COLUMN location;
                 ALTER TABLE calendar_invites DROP COLUMN description;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_raw;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_tzid;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_is_utc;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_is_date_only;
                 DROP TABLE pending_refetch;
                 DROP TABLE pending_rules;
                 DROP TABLE message_header_capture;
                 DROP TABLE message_headers;
                 DROP TABLE header_names;
                 DELETE FROM _migrations WHERE id IN (49, 50, 51, 52, 53, 54, 55, 56, 57);",
            )
            .unwrap();
            p.id
        };
        let db = Database::open(&path).expect("reopen applies 0049");
        assert_eq!(db.schema_version().unwrap(), latest_schema_version());
        assert_eq!(db.list_work_items(&pid).unwrap().len(), 1);
        assert_eq!(db.count_timeline(&pid).unwrap(), 0, "no backfill");
        db.record_decision(&pid, "after upgrade", None).unwrap();
        assert_eq!(db.count_timeline(&pid).unwrap(), 1);
        {
            let conn = db.lock_conn();
            assert!(conn
                .execute("UPDATE project_timeline SET summary = 'x'", [])
                .is_err());
        }
        db.delete_project(&pid).unwrap();
        drop(db);
        remove_db_and_sidecars(&path);
    }

    /// A v37 profile (0038–0042 reverted, the same fixture as the
    /// free-space-refusal test above — every id >= 38 must go because the
    /// runner's watermark is MAX(id)) has 38 through 42 pending, and the
    /// set is flagged heavy because of 0038.
    #[test]
    fn pending_migration_count_flags_a_pending_heavy_migration() {
        let path = tmp_db();
        {
            let db = Database::open(&path).expect("first open");
            let conn = db.lock_conn();
            conn.execute_batch(
                "DROP TRIGGER messages_before_delete_fts;
                 DROP TABLE message_fts_rowid;
                 DROP TABLE message_fts;
                 CREATE VIRTUAL TABLE message_fts USING fts5(
                     message_id UNINDEXED, subject, sender, body, tokenize = 'unicode61');
                 CREATE TRIGGER messages_after_delete_fts AFTER DELETE ON messages BEGIN
                     DELETE FROM message_fts WHERE message_id = OLD.id;
                 END;
                 DROP TABLE thread_dirty_messages;
                 ALTER TABLE thread_rebuild_state DROP COLUMN force_full;
                 ALTER TABLE sync_cursors DROP COLUMN descriptor_checkpoint_uid;
                 ALTER TABLE smart_mailboxes DROP COLUMN group_json;
                 ALTER TABLE smart_mailboxes DROP COLUMN scope_json;
                 DROP TABLE message_decode_skips;
                 ALTER TABLE saved_filters DROP COLUMN sort_order;
                 ALTER TABLE saved_filters DROP COLUMN stop_processing;
                 ALTER TABLE saved_filters DROP COLUMN enabled;
                 ALTER TABLE account_identities DROP COLUMN reply_to;
                 ALTER TABLE account_identities DROP COLUMN bcc;
                 ALTER TABLE account_identities DROP COLUMN is_default;
                 DROP TABLE tag_definitions;
                 DROP TABLE message_keywords;
                 DROP TABLE thread_mutes;
                 ALTER TABLE threads DROP COLUMN is_muted;
                 DROP TABLE work_items;
                 CREATE TABLE work_items (
                     id TEXT PRIMARY KEY,
                     project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                     title TEXT NOT NULL,
                     notes TEXT NOT NULL DEFAULT '',
                     state TEXT NOT NULL DEFAULT 'needs_reply'
                         CHECK (state IN ('needs_reply','waiting','delegated','blocked','done','someday')),
                     position INTEGER NOT NULL DEFAULT 0,
                     created_at INTEGER NOT NULL,
                     updated_at INTEGER NOT NULL
                 );
                 CREATE INDEX idx_work_items_project ON work_items(project_id, state, position);
                 CREATE TRIGGER links_work_item_delete AFTER DELETE ON work_items BEGIN
                     DELETE FROM links WHERE (src_type = 'work_item' AND src_id = OLD.id)
                                          OR (dst_type = 'work_item' AND dst_id = OLD.id);
                 END;
                 DROP TABLE suggestions;
                 DROP TABLE saved_views;
                 DROP TABLE project_timeline;
                 DROP INDEX idx_messages_account_rfc822;
                 DROP TABLE work_item_provenance;
                 DROP TABLE notification_outbox;
                 DROP TABLE vip_senders;
                 ALTER TABLE calendar_invites DROP COLUMN location;
                 ALTER TABLE calendar_invites DROP COLUMN description;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_raw;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_tzid;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_is_utc;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_is_date_only;
                 DROP TABLE pending_refetch;
                 DROP TABLE pending_rules;
                 DROP TABLE message_header_capture;
                 DROP TABLE message_headers;
                 DROP TABLE header_names;
                 DELETE FROM _migrations WHERE id IN (38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57);",
            )
            .unwrap();
        }

        let info = Database::pending_migration_count(&path).expect("probe must not fail");
        assert_eq!(
            info.applied_version, 37,
            "an EXISTING profile with 38 pending, not a from-scratch one — this is exactly the \
             case the Swift launch preflight must distinguish from a fresh install"
        );
        assert_eq!(
            info.pending_count,
            MIGRATIONS.iter().filter(|m| m.id > 37).count() as i64,
            "everything after 37 is pending (0038 through 0042; 8 is the only id gap)"
        );
        assert!(
            info.has_heavy,
            "0038 is among the pending migrations and is heavy"
        );

        // The probe never wrote anything: reopening still applies 38..=latest,
        // the same as if the probe had never run.
        let db = Database::open(&path).expect("reopen must still apply 38.. cleanly");
        assert_eq!(db.schema_version().unwrap(), latest_schema_version());
        drop(db);
        remove_db_and_sidecars(&path);
    }

    /// (3) A brand-new profile (nothing applied yet) gets no backup even
    /// when its very first migration fails — there's nothing to protect.
    #[test]
    fn a_brand_new_profile_gets_no_backup_even_on_a_failing_first_migration() {
        let path = tmp_db();
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .unwrap();
        conn.busy_timeout(Duration::from_secs(5)).unwrap();
        let db = Database {
            conn: Mutex::new(conn),
            reconciled_undo_send_accounts: Mutex::new(HashSet::new()),
            claimed_operations: Mutex::new(HashSet::new()),
            thread_rebuild_lock: Mutex::new(()),
            contacts_refresh_lock: Mutex::new(()),
            path: None,
        };
        let broken_first = [Migration {
            id: 1,
            name: "broken_first_migration",
            sql: "THIS IS NOT VALID SQL AT ALL;",
        }];
        let err = db
            .run_migration_set(&broken_first, 1, &path)
            .expect_err("must fail");
        match err {
            OpenError::MigrationFailed { backup_path, .. } => {
                assert!(
                    backup_path.is_none(),
                    "a brand-new profile must not get a pre-migration backup"
                );
            }
            other => panic!("expected MigrationFailed, got {other:?}"),
        }
        assert!(
            backup_files(&path).is_empty(),
            "no backup file must have been written for a brand-new profile"
        );
        drop(db);
        remove_db_and_sidecars(&path);
    }

    /// (4) Backup retention keeps only the newest 1, however many migration
    /// attempts (successful or not) ran against a non-empty profile — see
    /// DECISIONS.md for why 1 rather than 2 (two ~420MB copies, each
    /// carrying deleted mail, kept forever, was too much).
    #[test]
    fn backup_retention_keeps_only_the_newest_one() {
        let path = tmp_db();
        let db = Database::open(&path).expect("first open");
        let base = latest_schema_version();
        for i in 1..=3 {
            let extra = [Migration {
                id: base + i,
                name: "retention_probe",
                sql: "SELECT 1;",
            }];
            db.run_migration_set(&extra, base + i, &path)
                .expect("each probe migration succeeds");
        }
        let backups = backup_files(&path);
        assert_eq!(
            backups.len(),
            1,
            "retention must prune down to the newest 1 backup, found {backups:?}"
        );
        drop(db);
        remove_db_and_sidecars(&path);
    }

    /// The OTHER half of retention: a backup older than 30 days is pruned
    /// on the very next `open()`, whether or not that open triggers a
    /// migration — a profile that stays on the same schema for months must
    /// not keep a stale backup (and the deleted mail inside it) forever.
    #[test]
    fn backups_older_than_30_days_are_pruned_on_open() {
        let path = tmp_db();
        {
            let db = Database::open(&path).expect("first open");
            drop(db);
        }
        let (dir, stem) = Database::backup_dir_and_stem(&path);
        let stale = dir.join(format!("{stem}.pre-1.bak"));
        {
            let f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&stale)
                .expect("create a stand-in stale backup file");
            f.set_len(0).unwrap();
            let thirty_one_days_ago =
                std::time::SystemTime::now() - Duration::from_secs(31 * 24 * 60 * 60);
            f.set_modified(thirty_one_days_ago)
                .expect("backdate the stale backup's mtime");
        }
        assert!(
            stale.exists(),
            "sanity: the stand-in backup must exist before reopening"
        );

        let db = Database::open(&path).expect("reopen — nothing pending to migrate");
        assert!(
            !stale.exists(),
            "a backup older than 30 days must be pruned on open, even with no migration to run"
        );
        drop(db);
        remove_db_and_sidecars(&path);
    }

    /// (6) Defence in depth: a profile whose `_migrations` table names a
    /// DIFFERENT migration at an id this build also knows — e.g. two dev
    /// builds on parallel branches that each claimed the same next id —
    /// must be refused rather than silently built on top of.
    #[test]
    fn a_different_migration_name_at_a_known_id_is_refused() {
        let path = tmp_db();
        {
            let db = Database::open(&path).expect("first open");
            let conn = db.lock_conn();
            conn.execute(
                "UPDATE _migrations SET name = 'a_totally_different_migration' WHERE id = ?1",
                rusqlite::params![latest_schema_version()],
            )
            .unwrap();
        }
        match Database::open(&path) {
            Err(OpenError::SchemaTooNew { found, supported }) => {
                assert_eq!(found, latest_schema_version());
                assert_eq!(supported, latest_schema_version());
            }
            other => panic!(
                "expected SchemaTooNew for a mismatched migration name, got {}",
                open_result_debug(&other)
            ),
        }
        remove_db_and_sidecars(&path);
    }

    #[test]
    fn pre_0035_profile_upgrades_cleanly_and_regains_server_drafts_columns() {
        // A v34 profile in the wild: drop 0035's columns + watermark, then
        // reopen. The upgrade must re-add the five server-mirror columns on
        // `drafts`. ids 36 (contact_lookup_index, S64) and 37
        // (thread_rebuild_dirty, S64) also go: 36 landed after 35
        // (server_drafts, S36) when S64 merged main during the 2026-09-25
        // batch and had to renumber off the 35 S36 had already claimed, and
        // 37 followed it — leaving either in place would watermark past this
        // test and it would prove nothing.
        let path = tmp_db();
        {
            let db = Database::open(&path).expect("first open");
            let conn = db.lock_conn();
            conn.execute_batch(
                "ALTER TABLE drafts DROP COLUMN server_mailbox_path;
                 ALTER TABLE drafts DROP COLUMN server_uid;
                 ALTER TABLE drafts DROP COLUMN server_uid_validity;
                 ALTER TABLE drafts DROP COLUMN server_message_id;
                 ALTER TABLE drafts DROP COLUMN server_saved_at_epoch;
                 DROP INDEX idx_message_addresses_lower_address;
                 DROP TABLE thread_rebuild_state;
                 DROP TABLE thread_dirty_messages;
                 DROP TRIGGER messages_before_delete_fts;
                 DROP TABLE message_fts_rowid;
                 DROP TABLE message_fts;
                 CREATE VIRTUAL TABLE message_fts USING fts5(
                     message_id UNINDEXED, subject, sender, body, tokenize = 'unicode61');
                 CREATE TRIGGER messages_after_delete_fts AFTER DELETE ON messages BEGIN
                     DELETE FROM message_fts WHERE message_id = OLD.id;
                 END;
                 ALTER TABLE sync_cursors DROP COLUMN descriptor_checkpoint_uid;
                 ALTER TABLE smart_mailboxes DROP COLUMN group_json;
                 ALTER TABLE smart_mailboxes DROP COLUMN scope_json;
                 DROP TABLE message_decode_skips;
                 ALTER TABLE saved_filters DROP COLUMN sort_order;
                 ALTER TABLE saved_filters DROP COLUMN stop_processing;
                 ALTER TABLE saved_filters DROP COLUMN enabled;
                 ALTER TABLE account_identities DROP COLUMN reply_to;
                 ALTER TABLE account_identities DROP COLUMN bcc;
                 ALTER TABLE account_identities DROP COLUMN is_default;
                 DROP TABLE tag_definitions;
                 DROP TABLE message_keywords;
                 DROP INDEX idx_messages_account_rfc822;
                 DROP TABLE work_item_provenance;
                 DROP TABLE notification_outbox;
                 DROP TABLE vip_senders;
                 DROP TABLE thread_mutes;
                 ALTER TABLE threads DROP COLUMN is_muted;
                 DROP TABLE work_items;
                 CREATE TABLE work_items (
                     id TEXT PRIMARY KEY,
                     project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                     title TEXT NOT NULL,
                     notes TEXT NOT NULL DEFAULT '',
                     state TEXT NOT NULL DEFAULT 'needs_reply'
                         CHECK (state IN ('needs_reply','waiting','delegated','blocked','done','someday')),
                     position INTEGER NOT NULL DEFAULT 0,
                     created_at INTEGER NOT NULL,
                     updated_at INTEGER NOT NULL
                 );
                 CREATE INDEX idx_work_items_project ON work_items(project_id, state, position);
                 CREATE TRIGGER links_work_item_delete AFTER DELETE ON work_items BEGIN
                     DELETE FROM links WHERE (src_type = 'work_item' AND src_id = OLD.id)
                                          OR (dst_type = 'work_item' AND dst_id = OLD.id);
                 END;
                 DROP TABLE suggestions;
                 DROP TABLE saved_views;
                 DROP TABLE project_timeline;
                 ALTER TABLE calendar_invites DROP COLUMN location;
                 ALTER TABLE calendar_invites DROP COLUMN description;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_raw;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_tzid;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_is_utc;
                 ALTER TABLE calendar_invites DROP COLUMN recurrence_id_is_date_only;
                 DROP TABLE pending_refetch;
                 DROP TABLE pending_rules;
                 DROP TABLE message_header_capture;
                 DROP TABLE message_headers;
                 DROP TABLE header_names;
                 DELETE FROM _migrations WHERE id IN (35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57);",
            )
            .unwrap();
        }
        let db = Database::open(&path).expect("upgrade reopen");
        assert_eq!(db.schema_version().unwrap(), latest_schema_version());
        let conn = db.lock_conn();
        let cols: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('drafts')
                 WHERE name IN ('server_mailbox_path', 'server_uid',
                                 'server_uid_validity', 'server_message_id',
                                 'server_saved_at_epoch')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            cols, 5,
            "0035 re-adds all five server-mirror columns on upgrade"
        );
        drop(conn);
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
    }

    #[test]
    fn migration_created_expected_table() {
        let path = tmp_db();
        let db = Database::open(&path).expect("open");
        let conn = db.lock_conn();
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='app_meta'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1);
        drop(conn);
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
    }

    /// Every caller in this crate — production or test — must reach the
    /// connection through `Database::lock_conn()`, which recovers a poisoned
    /// mutex instead of propagating it (see the doc comment on `lock_conn`).
    /// Locking the field directly bypasses that recovery and reintroduces
    /// the 2026-07-11 Gmail-pilot brick-on-poison bug, so this scans every
    /// `.rs` file directly under `src/` (the migrations directory holds
    /// only `.sql`) and fails if that pattern shows up anywhere except the
    /// one line inside `lock_conn` itself. item 2.
    ///
    /// Whitespace (including newlines) between `.conn`, `.`, `lock` and `(`
    /// is normalised away before matching, so a call split across lines —
    /// e.g. `db.conn\n    .lock()\n    .unwrap()` — cannot hide from this
    /// scan the way a plain per-line `contains` check would miss it.
    ///
    /// The needle and the one allowed pattern are assembled at runtime (not
    /// written as a contiguous literal) so this test's own source does not
    /// trip its own scan.
    #[test]
    fn no_source_file_locks_the_connection_directly() {
        let needle: String = [".", "conn", ".", "lock", "("].concat();
        let allowed: String = [".", "conn", ".", "lock", "()", ".", "unwrap_or_else", "("].concat();
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(&src_dir).expect("read src dir") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let contents = std::fs::read_to_string(&path).expect("read source file");
            let is_lib_rs = path.file_name().and_then(|n| n.to_str()) == Some("lib.rs");

            // Strip whitespace but remember the original line number of each
            // kept character, so a match still points at a real line even
            // though the call may have been split across several.
            let mut normalised = String::with_capacity(contents.len());
            let mut line_of: Vec<usize> = Vec::with_capacity(contents.len());
            let mut line_no = 1usize;
            for ch in contents.chars() {
                if ch == '\n' {
                    line_no += 1;
                    continue;
                }
                if ch.is_whitespace() {
                    continue;
                }
                normalised.push(ch);
                line_of.push(line_no);
            }

            let mut search_from = 0usize;
            while let Some(off) = normalised[search_from..].find(&needle) {
                let start = search_from + off;
                // The one legitimate site: `lock_conn`'s own implementation.
                let is_allowed = is_lib_rs && normalised[start..].starts_with(&allowed);
                if !is_allowed {
                    let line = line_of.get(start).copied().unwrap_or(0);
                    offenders.push(format!("{}:{}", path.display(), line));
                }
                search_from = start + 1;
            }
        }
        assert!(
            offenders.is_empty(),
            "found a direct connection lock outside `Database::lock_conn()` (including any \
             split across lines) — use `lock_conn()` instead so a poisoned mutex is recovered \
             rather than bricking the profile:\n{}",
            offenders.join("\n")
        );
    }
}
