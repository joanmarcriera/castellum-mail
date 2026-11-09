// Castellum - lightweight in-process sync progress signal.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// A first sync against a large mailbox (Marc's pilot: 78,851 messages) can
// take minutes with nothing to show but an indeterminate spinner. This gives
// the UI a coarse, best-effort snapshot of "how far through" the CURRENT
// sync is, so it can show real numbers instead.
//
// Deliberately NOT a general progress framework: one snapshot per
// account_id, held in process memory only (never persisted — a relaunch
// naturally starts with none), overwritten in place by the sync loop and
// polled by the UI (there is no push channel across the C ABI). Concurrent
// syncs for different accounts don't collide (keyed by account_id); a second
// sync for the SAME account (there is never more than one in flight per
// account — `AppModel.syncGenerations` on the Swift side already prevents
// that) would simply overwrite the first's snapshot, which is the desired
// "most recent sync wins" behaviour.
//
// `total` for a mailbox comes from the IMAP `EXISTS` count captured at
// SELECT time (`MailboxState::exists`) — exact on a first sync (every
// message is "new"), but on an incremental sync `total` also counts
// already-synced messages the run won't refetch, so `fetched` will trail it
// even at completion. Good enough for "how big a job is this", which is the
// case this exists for.

use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Progress within the mailbox currently being fetched.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MailboxProgress {
    pub path: String,
    /// Messages persisted so far in this mailbox this run.
    pub fetched: i64,
    /// IMAP `EXISTS` for this mailbox at SELECT time.
    pub total: i64,
}

/// One account's current sync snapshot.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SyncProgress {
    pub account_id: String,
    /// Mailboxes fully visited (fetched, skipped, or failed) this run.
    pub mailboxes_completed: usize,
    /// Total mailboxes discovered this run (known once listing completes,
    /// which happens before any fetch, so this is stable from the start).
    pub mailboxes_total: usize,
    /// The mailbox currently being fetched, if any — `None` between
    /// mailboxes and once the pass has finished.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_mailbox: Option<MailboxProgress>,
    /// Sum of `fetched` across every mailbox visited so far this run
    /// (including the in-progress one).
    pub aggregate_fetched: i64,
    /// Sum of `total` across every mailbox visited so far this run
    /// (including the in-progress one) — a running lower bound that grows as
    /// later mailboxes are selected, not a final total, until `done`.
    pub aggregate_total: i64,
    /// Whether this run (success or failure) has finished. The snapshot is
    /// left in place with this set rather than removed, so a poll racing the
    /// FFI call's own return still sees the final numbers once.
    pub done: bool,
}

thread_local! {
    /// Set while this thread runs passes that must stay invisible to
    /// progress pollers (see [`quiet_this_thread`]).
    static QUIET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Guard returned by [`quiet_this_thread`]; progress reporting resumes for
/// the thread when it drops.
#[must_use = "progress is only suppressed while the guard is held"]
pub struct QuietGuard;

impl Drop for QuietGuard {
    fn drop(&mut self) {
        QUIET.with(|q| q.set(false));
    }
}

/// Make every progress mutator below a no-op on the CURRENT thread until the
/// guard drops. The IDLE supervisor's resync passes (S31b) run inside Rust,
/// unprompted by any Swift attempt, so publishing their snapshots would let
/// the app mistake them for an attempt's own: the account's early credential
/// save (`AppModel.startSyncProgressPolling`) treats a live snapshot as proof
/// that THE attempt it is polling for got past login, and an IDLE pass for the
/// same account is live whenever INBOX is being resynced. A Reconnect with a
/// wrong password overlapping such a wake would have its config saved to the
/// Keychain. A tiny INBOX delta needs no progress bar anyway.
pub fn quiet_this_thread() -> QuietGuard {
    QUIET.with(|q| q.set(true));
    QuietGuard
}

fn quiet() -> bool {
    QUIET.with(|q| q.get())
}

static PROGRESS: OnceLock<Mutex<HashMap<String, SyncProgress>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<String, SyncProgress>> {
    PROGRESS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Lock the registry, recovering from mutex poisoning — mirrors
/// `castellum_ffi::registry_lock` / `Database::lock_conn`: this is a plain
/// map of small `Copy`-ish snapshots, so a panic mid-update can't leave it
/// structurally torn, and treating poisoning as fatal would brick progress
/// reporting for every account after one unrelated panic.
fn lock() -> MutexGuard<'static, HashMap<String, SyncProgress>> {
    registry().lock().unwrap_or_else(|e| e.into_inner())
}

/// Reset the snapshot for `account_id` at the start of a sync pass, once the
/// mailbox list is known.
pub fn begin(account_id: &str, mailboxes_total: usize) {
    if quiet() {
        return;
    }
    lock().insert(
        account_id.to_string(),
        SyncProgress {
            account_id: account_id.to_string(),
            mailboxes_completed: 0,
            mailboxes_total,
            current_mailbox: None,
            aggregate_fetched: 0,
            aggregate_total: 0,
            done: false,
        },
    );
}

/// Start tracking a newly-selected mailbox: `total` is its IMAP `EXISTS`
/// count. A no-op if `begin` was never called for this account (e.g. a
/// caller that only wants the mailbox loop's bookkeeping in tests).
pub fn start_mailbox(account_id: &str, path: &str, total: i64) {
    if quiet() {
        return;
    }
    let mut map = lock();
    if let Some(p) = map.get_mut(account_id) {
        p.current_mailbox = Some(MailboxProgress {
            path: path.to_string(),
            fetched: 0,
            total,
        });
        p.aggregate_total += total;
    }
}

/// Record `n` more messages persisted in the mailbox currently being
/// tracked. Cheap (an uncontended mutex, no I/O) — safe to call per message.
pub fn advance(account_id: &str, n: i64) {
    if n == 0 || quiet() {
        return;
    }
    let mut map = lock();
    if let Some(p) = map.get_mut(account_id) {
        if let Some(mb) = p.current_mailbox.as_mut() {
            mb.fetched += n;
        }
        p.aggregate_fetched += n;
    }
}

/// Mark the current mailbox visited (rolled into `mailboxes_completed`),
/// whichever way the loop left it — fetched, skipped, or errored. Called via
/// `MailboxProgressGuard`'s `Drop` in `sync.rs` so every exit path (an early
/// `continue` on error included) accounts for it exactly once.
pub fn finish_mailbox(account_id: &str) {
    if quiet() {
        return;
    }
    let mut map = lock();
    if let Some(p) = map.get_mut(account_id) {
        p.mailboxes_completed += 1;
        p.current_mailbox = None;
    }
}

/// Mark the whole sync pass finished, success or failure. See the `done`
/// field doc for why the snapshot stays rather than being removed.
pub fn finish(account_id: &str) {
    if quiet() {
        return;
    }
    let mut map = lock();
    if let Some(p) = map.get_mut(account_id) {
        p.current_mailbox = None;
        p.done = true;
    }
}

/// The current snapshot for `account_id`, if any sync has run for it this
/// process. `None` before the first sync — never an error; there is simply
/// nothing to report yet.
pub fn snapshot(account_id: &str) -> Option<SyncProgress> {
    lock().get(account_id).cloned()
}

/// `snapshot` as a JSON string (`null` when there is no snapshot yet) for
/// the FFI boundary. Never fails: serializing this small, non-cyclic struct
/// cannot error in practice, but a hypothetical failure still degrades to
/// `"null"` rather than surfacing as a sync error over an unrelated call.
pub fn snapshot_json(account_id: &str) -> Result<String, crate::CoreError> {
    Ok(serde_json::to_string(&snapshot(account_id)).unwrap_or_else(|_| "null".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each test uses its own account id (this is a process-global registry,
    // and `cargo test` runs tests in threads within one process).

    #[test]
    fn begin_then_snapshot_reflects_empty_state() {
        begin("acct-begin", 3);
        let snap = snapshot("acct-begin").expect("snapshot after begin");
        assert_eq!(snap.mailboxes_total, 3);
        assert_eq!(snap.mailboxes_completed, 0);
        assert!(snap.current_mailbox.is_none());
        assert_eq!(snap.aggregate_fetched, 0);
        assert!(!snap.done);
    }

    #[test]
    fn mailbox_lifecycle_advances_and_completes() {
        begin("acct-lifecycle", 2);
        start_mailbox("acct-lifecycle", "INBOX", 100);
        advance("acct-lifecycle", 1);
        advance("acct-lifecycle", 4);
        let mid = snapshot("acct-lifecycle").unwrap();
        let mb = mid.current_mailbox.expect("current mailbox set");
        assert_eq!(mb.path, "INBOX");
        assert_eq!(mb.fetched, 5);
        assert_eq!(mb.total, 100);
        assert_eq!(mid.aggregate_fetched, 5);
        assert_eq!(mid.aggregate_total, 100);

        finish_mailbox("acct-lifecycle");
        let after = snapshot("acct-lifecycle").unwrap();
        assert!(after.current_mailbox.is_none());
        assert_eq!(after.mailboxes_completed, 1);
        // aggregate counters persist across mailboxes.
        assert_eq!(after.aggregate_fetched, 5);

        start_mailbox("acct-lifecycle", "Archive", 50);
        advance("acct-lifecycle", 2);
        finish_mailbox("acct-lifecycle");
        let after2 = snapshot("acct-lifecycle").unwrap();
        assert_eq!(after2.mailboxes_completed, 2);
        assert_eq!(after2.aggregate_fetched, 7);
        assert_eq!(after2.aggregate_total, 150);
    }

    #[test]
    fn finish_marks_done_and_clears_current_mailbox() {
        begin("acct-finish", 1);
        start_mailbox("acct-finish", "INBOX", 10);
        advance("acct-finish", 3);
        finish("acct-finish");
        let snap = snapshot("acct-finish").unwrap();
        assert!(snap.done);
        assert!(snap.current_mailbox.is_none());
        // finish() doesn't discard progress already recorded.
        assert_eq!(snap.aggregate_fetched, 3);
    }

    #[test]
    fn snapshot_of_unknown_account_is_none() {
        assert!(snapshot("acct-never-synced").is_none());
    }

    #[test]
    fn advance_and_finish_mailbox_before_begin_are_harmless_no_ops() {
        // No `begin()` call for this account — every mutator must no-op
        // rather than panic or fabricate an entry.
        advance("acct-no-begin", 5);
        finish_mailbox("acct-no-begin");
        start_mailbox("acct-no-begin", "INBOX", 10);
        assert!(snapshot("acct-no-begin").is_none());
    }
}
