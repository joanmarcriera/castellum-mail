// Castellum - concurrent first-sync contention bench.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Reproduces the 2026-09-26 GUI observation without a GUI or a mail server:
// two accounts run their FIRST sync at the same time into ONE profile while
// the contacts aggregate is refreshed repeatedly (in the app every completed
// sync schedules `refreshContactsDebounced`, so with several accounts
// syncing a refresh is almost always running or about to), and a "UI" thread
// times a cheap read that needs the single DB connection — the same shape as
// the Settings/progress-poller calls that froze.
//
// Drives the REAL `sync::sync_with_provider` against `perf_common`'s
// synthetic multi-account corpus (the same data `seed_profile` and the perf
// gates use), so it needs no Docker and prints only counts and timings.
//
// Usage:
//   cargo run --release -p castellum-core --example concurrent_first_sync
//   CFS_TOTAL=100000   total messages across both accounts (default 100000)
//   CFS_REFRESH_GAP_MS=250  pause between back-to-back contacts refreshes
//   CFS_REFRESH=0      no refreshes during the syncs (isolates the syncs'
//                      own contention from the refresh's)
//
// What it prints (all times in ms):
//   * per-account first-sync wall time, and the same sync run ALONE first
//     (the uncontended baseline) so the slowdown is a ratio, not a guess;
//   * every contacts refresh: count, p50/max elapsed, and p50/max of the
//     longest single hold of the DB connection inside one refresh
//     (`AggregateReport.max_lock_hold_ms`);
//   * UI-read latency during the concurrent phase: p50/p99/max.
//   * a final standalone refresh over the whole profile with the UI reader
//     running — the "Settings window freezes for X ms" number.

#[path = "perf_common/mod.rs"]
mod perf_common;

use castellum_core::sync::sync_with_provider;
use castellum_db::Database;
use perf_common::{corpus_layout, pct, Rng, SyntheticProvider};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const NOW: i64 = 1_800_000_000;

fn env_i64(key: &str, default: i64) -> i64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn fresh_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "castellum-cfs-{tag}-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create bench dir");
    dir
}

/// Background "UI" probe: a cheap read that needs the DB connection, every
/// 20 ms, until `stop`. Returns latencies in microseconds.
fn spawn_reader(db: Arc<Database>, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<Vec<u64>> {
    std::thread::spawn(move || {
        let mut lat = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            let t = Instant::now();
            let _ = db.mailbox_counts("acct-a");
            lat.push(t.elapsed().as_micros() as u64);
            std::thread::sleep(Duration::from_millis(20));
        }
        lat
    })
}

fn sync_one(db: &Database, account: &perf_common::AccountSpec, seed: u64) -> Duration {
    let mut rng = Rng::new(seed);
    let mut provider = SyntheticProvider::new(account, &mut rng, false);
    let started = Instant::now();
    sync_with_provider(
        db,
        account.id,
        account.email,
        None,
        &mut provider,
        "synthetic-host",
    )
    .expect("synthetic first sync");
    started.elapsed()
}

fn ms(us: u64) -> f64 {
    us as f64 / 1000.0
}

fn summary(label: &str, mut v: Vec<u64>) {
    if v.is_empty() {
        println!("{label}: no samples");
        return;
    }
    v.sort_unstable();
    println!(
        "{label}: n={} p50={:.1}ms p99={:.1}ms max={:.1}ms",
        v.len(),
        ms(pct(&v, 0.50)),
        ms(pct(&v, 0.99)),
        ms(*v.last().unwrap())
    );
}

fn main() {
    let total = env_i64("CFS_TOTAL", 100_000);
    let gap = Duration::from_millis(env_i64("CFS_REFRESH_GAP_MS", 250) as u64);
    let layout = corpus_layout(total);
    assert_eq!(layout.len(), 2, "corpus_layout is two accounts");
    println!("concurrent_first_sync: total={total} accounts=2 refresh_gap={gap:?}");

    // 1. Uncontended baselines: each account alone, no refresh, no reader.
    let mut alone = Vec::new();
    for (i, acct) in layout.iter().enumerate() {
        let dir = fresh_dir(&format!("alone-{i}"));
        let db = Database::open(&dir.join("castellum.db")).expect("open");
        let wall = sync_one(&db, acct, 42 + i as u64);
        println!(
            "alone {}: first sync {:.0}ms",
            acct.id,
            wall.as_secs_f64() * 1e3
        );
        alone.push(wall);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 2. Concurrent: both first syncs + back-to-back refreshes + UI reader.
    let dir = fresh_dir("concurrent");
    let db = Arc::new(Database::open(&dir.join("castellum.db")).expect("open"));
    let stop = Arc::new(AtomicBool::new(false));
    let syncing = Arc::new(AtomicUsize::new(layout.len()));
    let reader = spawn_reader(Arc::clone(&db), Arc::clone(&stop));
    let refresher = {
        let db = Arc::clone(&db);
        let syncing = Arc::clone(&syncing);
        std::thread::spawn(move || {
            let (mut elapsed, mut hold) = (Vec::new(), Vec::new());
            let enabled = env_i64("CFS_REFRESH", 1) != 0;
            while enabled && syncing.load(Ordering::Relaxed) > 0 {
                let t = Instant::now();
                let r = db.refresh_contact_aggregates(NOW).expect("refresh");
                elapsed.push(t.elapsed().as_micros() as u64);
                hold.push(r.max_lock_hold_ms * 1000);
                std::thread::sleep(gap);
            }
            (elapsed, hold)
        })
    };
    let layout = Arc::new(layout);
    let mut handles = Vec::new();
    for i in 0..layout.len() {
        let db = Arc::clone(&db);
        let layout = Arc::clone(&layout);
        let syncing = Arc::clone(&syncing);
        handles.push(std::thread::spawn(move || {
            let wall = sync_one(&db, &layout[i], 42 + i as u64);
            syncing.fetch_sub(1, Ordering::Relaxed);
            wall
        }));
    }
    for (i, h) in handles.into_iter().enumerate() {
        let wall = h.join().expect("sync thread");
        println!(
            "concurrent {}: first sync {:.0}ms ({:.2}x alone)",
            layout[i].id,
            wall.as_secs_f64() * 1e3,
            wall.as_secs_f64() / alone[i].as_secs_f64()
        );
    }
    let (elapsed, hold) = refresher.join().expect("refresher");
    stop.store(true, Ordering::Relaxed);
    let reads = reader.join().expect("reader");
    summary("concurrent refresh elapsed", elapsed);
    summary("concurrent refresh max_lock_hold", hold);
    summary("concurrent UI read", reads);

    // 3. One standalone refresh over the finished profile, reader running.
    let stop = Arc::new(AtomicBool::new(false));
    let reader = spawn_reader(Arc::clone(&db), Arc::clone(&stop));
    std::thread::sleep(Duration::from_millis(100));
    let r = db.refresh_contact_aggregates(NOW).expect("final refresh");
    std::thread::sleep(Duration::from_millis(100));
    stop.store(true, Ordering::Relaxed);
    let reads = reader.join().expect("reader");
    println!(
        "final refresh: contacts={} edges={} elapsed={}ms max_lock_hold={}ms",
        r.contacts_upserted, r.edges_rebuilt, r.elapsed_ms, r.max_lock_hold_ms
    );
    summary("final-refresh UI read", reads);
    drop(db);
    let _ = std::fs::remove_dir_all(Path::new(&dir));
}
