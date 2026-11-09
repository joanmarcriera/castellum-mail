// Castellum - per-account sync-pass exclusion (S31b).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// A sync pass for one account must never overlap another pass for the SAME
// account, whatever triggered either: a launch/manual/periodic pass driven by
// Swift (`sync_imap_json`), or an IDLE wake resyncing INBOX from the
// supervisor thread (`idle.rs`). Swift's own coalescing (`inFlightSyncs`,
// `queuedResync`) cannot see an IDLE pass — it runs entirely inside Rust —
// and two overlapping passes both read the same cursor and both treat the
// same UID as new: the message is counted twice, arrival rules and
// mute-on-arrival fire twice, and `sync_progress` snapshots interleave.
//
// The gate is a blocking, per-account critical section. A trigger that
// arrives while a pass is running WAITS and then runs its own pass: the
// follow-up the overlap made necessary, never dropped, never concurrent. The
// follow-up pass starts from the cursor the first one committed, so what the
// first pass already imported is not new to it.
//
// Usage: `let _pass = pass_gate::acquire(scope_of(db), account_id);` and hold for
// the whole pass; dropping it (including on panic unwinding) releases the
// account. `wait_until_idle` lets a thread that must stay responsive to a
// shutdown (the IDLE supervisor) wait in short interruptible slices first.

use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// What is exclusive: one account within one open store. Keyed by the
/// `Database` instance, not the account id alone: the exclusion exists
/// because passes share a cursor and the rows it guards, and that is per
/// store. Two stores that merely share an account id (two profiles, or a test
/// simulating a relaunch against the same directory) cannot interfere.
type Key = (usize, String);

/// The gate scope for `db`: its address, stable for as long as the store is
/// open (a pass holds the store borrowed, so it cannot move or drop mid-pass).
pub fn scope_of(db: &castellum_db::Database) -> usize {
    db as *const castellum_db::Database as usize
}

struct Gate {
    running: Mutex<Vec<Key>>,
    released: Condvar,
}

static GATE: Gate = Gate {
    running: Mutex::new(Vec::new()),
    released: Condvar::new(),
};

/// Lock the set, recovering from poisoning: it is a plain set of ids, so a
/// panic elsewhere cannot leave it structurally torn (same reasoning as
/// `sync_progress::lock`).
fn lock() -> MutexGuard<'static, Vec<Key>> {
    GATE.running.lock().unwrap_or_else(|e| e.into_inner())
}

/// Held for the duration of one account's sync pass.
#[must_use = "the account is only exclusive while the guard is held"]
pub struct PassGuard {
    key: Key,
}

impl Drop for PassGuard {
    fn drop(&mut self) {
        lock().retain(|k| k != &self.key);
        GATE.released.notify_all();
    }
}

fn is_busy(running: &[Key], scope: usize, account_id: &str) -> bool {
    running
        .iter()
        .any(|(s, id)| *s == scope && id == account_id)
}

/// Block until no pass for `account_id` is running in `scope`, then claim it.
pub fn acquire(scope: usize, account_id: &str) -> PassGuard {
    let mut running = lock();
    while is_busy(&running, scope, account_id) {
        running = GATE
            .released
            .wait(running)
            .unwrap_or_else(|e| e.into_inner());
    }
    let key = (scope, account_id.to_string());
    running.push(key.clone());
    PassGuard { key }
}

/// Whether a pass for `account_id` is running right now in `scope`.
pub fn is_running(scope: usize, account_id: &str) -> bool {
    is_busy(&lock(), scope, account_id)
}

/// Wait (in `slice`-long steps) until no pass for `account_id` is running,
/// calling `abort` between steps; returns `false` if `abort` said stop.
/// Only a courtesy for callers that must stay interruptible — the real
/// exclusion is `acquire`, so losing a race after this returns just blocks
/// briefly in `acquire` instead of overlapping.
pub fn wait_until_idle(
    scope: usize,
    account_id: &str,
    slice: Duration,
    mut abort: impl FnMut() -> bool,
) -> bool {
    loop {
        {
            let running = lock();
            if !is_busy(&running, scope, account_id) {
                return true;
            }
            let (_running, _) = GATE
                .released
                .wait_timeout(running, slice)
                .unwrap_or_else(|e| e.into_inner());
        }
        if abort() {
            return false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn a_second_acquire_waits_for_the_first_to_drop() {
        let first = acquire(1, "gate-a");
        assert!(is_running(1, "gate-a"));
        let entered = Arc::new(AtomicUsize::new(0));
        let flag = Arc::clone(&entered);
        let waiter = thread::spawn(move || {
            let _second = acquire(1, "gate-a");
            flag.fetch_add(1, Ordering::SeqCst);
        });
        thread::sleep(Duration::from_millis(100));
        assert_eq!(entered.load(Ordering::SeqCst), 0, "must wait while held");
        drop(first);
        waiter.join().unwrap();
        assert_eq!(entered.load(Ordering::SeqCst), 1, "runs once released");
        assert!(!is_running(1, "gate-a"));
    }

    #[test]
    fn different_accounts_and_different_stores_do_not_block_each_other() {
        let _a = acquire(2, "gate-b1");
        let _b = acquire(2, "gate-b2");
        let _c = acquire(3, "gate-b1");
        assert!(is_running(2, "gate-b1") && is_running(2, "gate-b2") && is_running(3, "gate-b1"));
    }

    #[test]
    fn a_panicking_pass_still_releases_the_account() {
        let result = thread::spawn(|| {
            let _g = acquire(4, "gate-c");
            panic!("pass blew up");
        })
        .join();
        assert!(result.is_err());
        assert!(!is_running(4, "gate-c"));
    }

    #[test]
    fn wait_until_idle_can_be_aborted() {
        let _held = acquire(5, "gate-d");
        let mut polls = 0;
        let done = wait_until_idle(5, "gate-d", Duration::from_millis(10), || {
            polls += 1;
            polls >= 3
        });
        assert!(!done, "abort wins while the account stays busy");
    }
}
