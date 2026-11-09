// Castellum - last-sync KPI snapshot ( follow-up).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// `sync_timing` is an opt-in, in-memory, thread-local probe: its numbers only
// exist for the caller that enabled it, and only until that call returns. For
// a KPI to be checkable LATER — by a future Diagnostics UI, without a sync in
// progress — the numbers need to survive past the syncing call. This module
// is that persistence step: after `Profile::sync_imap_json` completes, it
// writes a compact summary of whatever `sync_timing` recorded to a JSON file
// in the profile directory (alongside the SQLite DB/FTS index the profile
// already owns), and offers a cheap read-back for the FFI call that exposes
// it to Swift (`castellum_last_sync_kpi`).
//
// Discipline matches project-docs/perf/2026-09-sync-baseline.md: only counts,
// byte totals and timings ever land here — no message content, no mailbox
// names or paths, no server identifiers. `account_id` is the app's own local
// account id (e.g. "bench-acc" in tests), never the email address.
//
// Cost discipline: this is a per-SYNC step, not a per-message one. Building
// the snapshot sorts each phase's sample vector once (already collected by
// `sync_timing` during the sync that just ran) and writes one small JSON
// file; reading it back is a single file read, no parsing beyond what the
// caller does with the returned string.

use crate::sync_timing::SyncTimings;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Filename inside the profile directory. Deliberately not a dotfile — a
/// support bundle or a curious `ls` of the profile directory should see it.
pub const LAST_SYNC_KPI_FILENAME: &str = "last_sync_kpi.json";

#[derive(Serialize)]
struct PhaseKpi {
    /// Number of samples recorded for this phase. Most phases record one
    /// sample per message; `db.upsert_descriptor` is the exception since
    /// -- one sample per `DESCRIPTOR_UPSERT_BATCH_SIZE`
    /// (200)-message transaction, not one per message.
    count: usize,
    total_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
}

#[derive(Serialize)]
struct LastSyncKpiSnapshot<'a> {
    /// Always true for a snapshot this module wrote; lets a reader
    /// distinguish "file missing" (no sync has completed yet) without a
    /// separate error channel, since `read_last_sync_kpi` returns `None` for
    /// that case anyway. Kept for forward compatibility with a future
    /// "written but empty" representation.
    available: bool,
    account_id: &'a str,
    /// Unix epoch seconds when this snapshot was written.
    timestamp_unix: i64,
    /// Total new messages imported this sync (sum of every mailbox's
    /// `fetched`); 0 on a no-op resync or a failed sync.
    message_count: i64,
    wall_ms: f64,
    phases: BTreeMap<&'static str, PhaseKpi>,
}

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx]
}

fn phase_kpi(samples: &[u64]) -> PhaseKpi {
    let mut s = samples.to_vec();
    s.sort_unstable();
    PhaseKpi {
        count: s.len(),
        total_ms: s.iter().sum::<u64>() as f64 / 1000.0,
        p50_ms: pct(&s, 0.5) as f64 / 1000.0,
        p95_ms: pct(&s, 0.95) as f64 / 1000.0,
        max_ms: *s.last().unwrap_or(&0) as f64 / 1000.0,
    }
}

/// Unix epoch seconds now (0 on a pre-1970 clock; only skews the timestamp).
fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Write a compact last-run KPI snapshot to `profile_dir/last_sync_kpi.json`.
///
/// Writes to a temp file in the same directory and renames into place, so a
/// concurrent reader (`read_last_sync_kpi`, called from the FFI at any time,
/// including mid-sync) never observes a partially written file — it sees
/// either the previous snapshot or this one, never a half-written one.
///
/// Callers treat this as diagnostics-only: an I/O error here must never fail
/// (or be allowed to slow down the outcome of) the sync it is reporting on.
pub fn write_last_sync_kpi(
    profile_dir: &Path,
    account_id: &str,
    message_count: i64,
    wall: std::time::Duration,
    timings: &SyncTimings,
) -> std::io::Result<()> {
    let phases: BTreeMap<&'static str, PhaseKpi> = timings
        .samples
        .iter()
        .map(|(phase, samples)| (*phase, phase_kpi(samples)))
        .collect();
    let snapshot = LastSyncKpiSnapshot {
        available: true,
        account_id,
        timestamp_unix: now_epoch(),
        message_count,
        wall_ms: wall.as_secs_f64() * 1000.0,
        phases,
    };
    let json = serde_json::to_string(&snapshot)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    // pid-scoped temp name: this profile is single-writer today, but a
    // crashed previous write must never leave a half-written temp file that
    // a later one could collide with or be confused for.
    let tmp = profile_dir.join(format!(
        ".{LAST_SYNC_KPI_FILENAME}.{}.tmp",
        std::process::id()
    ));
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, profile_dir.join(LAST_SYNC_KPI_FILENAME))
}

/// Read back the last-run KPI snapshot as raw JSON text, or `None` if no sync
/// has completed in this profile yet. A plain file read — no parsing, no
/// computation — so it is cheap to call at any time, including while another
/// sync is in progress (it just returns the PREVIOUS run's numbers).
pub fn read_last_sync_kpi(profile_dir: &Path) -> Option<String> {
    std::fs::read_to_string(profile_dir.join(LAST_SYNC_KPI_FILENAME)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync_timing;

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        sync_timing::enable();
        sync_timing::time("db.upsert_descriptor", || ());
        sync_timing::time("db.upsert_descriptor", || ());
        let t = sync_timing::take().unwrap();

        write_last_sync_kpi(
            dir.path(),
            "acc-1",
            2,
            std::time::Duration::from_millis(5),
            &t,
        )
        .unwrap();

        let json = read_last_sync_kpi(dir.path()).expect("snapshot written");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["available"], true);
        assert_eq!(v["account_id"], "acc-1");
        assert_eq!(v["message_count"], 2);
        assert_eq!(v["phases"]["db.upsert_descriptor"]["count"], 2);
    }

    #[test]
    fn missing_snapshot_reads_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_last_sync_kpi(dir.path()).is_none());
    }

    #[test]
    fn a_later_write_replaces_the_earlier_one() {
        let dir = tempfile::tempdir().unwrap();
        sync_timing::enable();
        let t1 = sync_timing::take().unwrap();
        write_last_sync_kpi(dir.path(), "acc-1", 1, std::time::Duration::ZERO, &t1).unwrap();

        sync_timing::enable();
        let t2 = sync_timing::take().unwrap();
        write_last_sync_kpi(dir.path(), "acc-2", 2, std::time::Duration::ZERO, &t2).unwrap();

        let json = read_last_sync_kpi(dir.path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["account_id"], "acc-2");
        assert_eq!(v["message_count"], 2);
    }
}
