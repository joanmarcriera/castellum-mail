// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Two accounts doing their FIRST sync at the same time into ONE profile,
// against a local GreenMail server.
//
// #[ignore] by default: requires the GreenMail container from scripts/local-imap.
// Run locally with:
//   docker-compose -f scripts/local-imap/docker-compose.yml up -d
//   cargo test -p castellum-core --test concurrent_first_sync_greenmail -- --ignored --nocapture
//
// The app shape this mirrors: several accounts' first syncs overlap, every
// completed sync schedules a contacts refresh, and the UI keeps polling the
// core. Before S72 the refresh held the profile's single DB connection for
// its whole pass, so the syncs and the UI queued behind it. This test runs
// both syncs, a back-to-back refresher and a "UI" reader together and
// asserts that both first syncs complete in bounded wall time, that the
// reader is never shut out for long, and that no refresh holds the
// connection for long. Each run provisions its own two accounts (per-run
// tag), so it is parallel-safe and never depends on pre-existing contents.

mod common;

use castellum_core::open_profile;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Messages delivered to each account before the syncs start.
const PER_ACCOUNT: usize = 120;
/// Generous bounds: GreenMail on a loaded CI runner is slow, but a starved
/// sync (the bug) is minutes, not seconds.
const SYNC_WALL_BOUND: Duration = Duration::from_secs(90);
const READ_BOUND: Duration = Duration::from_secs(3);
const LOCK_HOLD_BOUND_MS: u64 = 1_000;

fn deliver_corpus(email: &str, tag: &str) {
    for i in 0..PER_ACCOUNT {
        // Distinct senders plus a shared cc, so the refresh has contacts and
        // edges to write while the syncs run.
        let from = format!("sender{}-{tag}@example.test", i % 40);
        let body = format!(
            "From: Sender {i} <{from}>\r\nTo: <{email}>\r\nCc: <shared-{tag}@example.test>\r\n\
             Subject: concurrent first sync {i}\r\nMessage-ID: <cfs-{tag}-{i}@example.test>\r\n\r\n\
             body {i}\r\n"
        );
        common::smtp_deliver(&from, email, &body);
    }
}

fn inbox_descriptors(report_json: &str) -> u64 {
    let report: Value = serde_json::from_str(report_json).expect("sync report json");
    report["mailboxes"]
        .as_array()
        .expect("mailboxes")
        .iter()
        .find(|m| m["path"].as_str() == Some("INBOX"))
        .and_then(|m| m["descriptors_fetched"].as_u64())
        .expect("INBOX descriptors_fetched")
}

#[test]
#[ignore = "requires GreenMail (scripts/local-imap)"]
fn two_concurrent_first_syncs_both_complete_and_neither_starves() {
    let tag = common::run_tag("cfs");
    let email_a = common::provision(&format!("{tag}-a"));
    let email_b = common::provision(&format!("{tag}-b"));
    deliver_corpus(&email_a, &format!("{tag}-a"));
    deliver_corpus(&email_b, &format!("{tag}-b"));

    let dir = castellum_testutil::scratch_root().join(format!("castellum-cfs-e2e-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    let profile = Arc::new(open_profile(&dir).expect("open profile"));

    let syncing = Arc::new(AtomicUsize::new(2));
    let stop = Arc::new(AtomicBool::new(false));

    // UI reader: a cheap call that needs the DB connection, every 20 ms.
    let reader = {
        let (profile, stop) = (Arc::clone(&profile), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut worst = Duration::ZERO;
            while !stop.load(Ordering::Relaxed) {
                let t = Instant::now();
                profile.list_accounts_json().expect("list accounts");
                worst = worst.max(t.elapsed());
                std::thread::sleep(Duration::from_millis(20));
            }
            worst
        })
    };
    // Refresher: back-to-back contacts refreshes while either sync runs.
    let refresher = {
        let (profile, syncing) = (Arc::clone(&profile), Arc::clone(&syncing));
        std::thread::spawn(move || {
            let (mut runs, mut worst_hold) = (0u32, 0u64);
            while syncing.load(Ordering::Relaxed) > 0 {
                let r: Value =
                    serde_json::from_str(&profile.refresh_contacts_json().expect("refresh"))
                        .expect("aggregate json");
                worst_hold = worst_hold.max(r["max_lock_hold_ms"].as_u64().unwrap_or(0));
                runs += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            (runs, worst_hold)
        })
    };

    let started = Instant::now();
    let syncs: Vec<_> = [("cfs-a", email_a), ("cfs-b", email_b)]
        .into_iter()
        .map(|(id, email)| {
            let (profile, syncing) = (Arc::clone(&profile), Arc::clone(&syncing));
            std::thread::spawn(move || {
                let t = Instant::now();
                let report = profile
                    .sync_imap_json(&common::sync_config_json(id, &email))
                    .expect("first sync");
                syncing.fetch_sub(1, Ordering::Relaxed);
                (id, report, t.elapsed())
            })
        })
        .collect();
    let results: Vec<_> = syncs
        .into_iter()
        .map(|h| h.join().expect("sync thread"))
        .collect();
    let wall = started.elapsed();
    let (refresh_runs, worst_hold) = refresher.join().expect("refresher");
    stop.store(true, Ordering::Relaxed);
    let worst_read = reader.join().expect("reader");

    for (id, report, took) in &results {
        let n = inbox_descriptors(report);
        println!("{id}: {n} descriptors in {took:?}");
        // PER_ACCOUNT + the provisioning bootstrap message.
        assert!(n > PER_ACCOUNT as u64, "{id} synced only {n} messages");
        assert!(*took < SYNC_WALL_BOUND, "{id} first sync took {took:?}");
    }
    println!(
        "both: {wall:?}; refreshes={refresh_runs} worst_hold={worst_hold}ms worst_ui_read={worst_read:?}"
    );
    assert!(
        refresh_runs >= 1,
        "the refresher never ran alongside the syncs"
    );
    assert!(
        worst_hold < LOCK_HOLD_BOUND_MS,
        "a refresh held the DB connection for {worst_hold} ms"
    );
    assert!(
        worst_read < READ_BOUND,
        "the UI reader waited {worst_read:?} for the connection"
    );

    // Both accounts are in the profile; the final aggregate sees both.
    let accounts: Value =
        serde_json::from_str(&profile.list_accounts_json().expect("accounts")).unwrap();
    for id in ["cfs-a", "cfs-b"] {
        assert!(
            accounts
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["id"].as_str() == Some(id)),
            "account {id} missing"
        );
    }
    drop(profile);
    let _ = std::fs::remove_dir_all(&dir);
}
