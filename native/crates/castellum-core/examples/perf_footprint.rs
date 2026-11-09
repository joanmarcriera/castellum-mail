// Castellum - memory and on-disk footprint of the seeded 100k profile (M2).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// `read_perf_gate` times the read path; nothing else measured what the same
// profile COSTS: how big the database is, how large the WAL grows, and how
// much memory a process needs to open it and run the list, search and counts
// paths. This example reports exactly that, as ADVISORY `PERF` rows (no target,
// no ceiling: both print as `inf`, every row reads PASS) through
// `perf_common::perf_line`, so with `CASTELLUM_PERF_JSONL` set they land in the
// same JSONL history as the budgeted rows and the trend lane
// can follow them.
//
// Usage:
//   cargo run --release -p castellum-core --example seed_profile -- <dir>
//   cargo run --release -p castellum-core --example perf_footprint -- <dir>
//
// `<dir>` is a profile from `seed_profile` (default body-indexed mode; a
// `--descriptor-only` profile works too and shows what bodies cost). The
// profile is copied to a scratch directory first: opening runs migrations and
// the WAL, and the fixture is never mutated.
//
// Rows (all advisory):
//   footprint_db_file_size            MiB   castellum.db after a TRUNCATE
//                                           checkpoint (seeder-measured)
//   footprint_wal_after_seed          MiB   WAL as the last seed write left it
//   footprint_wal_after_checkpoint    MiB   WAL after wal_checkpoint(TRUNCATE)
//   footprint_bytes_per_message       KiB/message  main file / message count
//   footprint_peak_rss_open           MiB   peak resident set after open_profile
//   footprint_peak_rss_launch_list    MiB   ... after the launch reads + list pages
//   footprint_peak_rss_search         MiB   ... after the four search shapes
//   footprint_peak_rss_counts         MiB   ... after the counts rows
// The peak is cumulative over the process (getrusage ru_maxrss), so each row
// is "the high-water mark so far"; the last one is the whole run's peak.
// The file-size and WAL rows come from the manifest the seeder wrote while its
// connection was still open (a WAL cannot be read after a clean close: it is
// gone); a profile seeded before M2 has no such fields and those rows are
// skipped with an INFO line.
//
// No message content is printed: only byte counts, MiB and the harness's own
// synthetic query terms.

#[path = "perf_common/mod.rs"]
mod perf_common;

use castellum_core::open_profile;
use perf_common::{corpus_layout, perf_line, Budget};
use std::path::{Path, PathBuf};

const MIB: f64 = 1024.0 * 1024.0;

/// An advisory row: no target, no ceiling, so it can never WARN or FAIL.
fn advisory_row(name: &'static str, unit: &'static str) -> Budget {
    Budget {
        name,
        unit,
        target: f64::INFINITY,
        ceiling: f64::INFINITY,
    }
}

/// `bytes` as MiB.
fn mib(bytes: u64) -> f64 {
    bytes as f64 / MIB
}

/// Main database bytes per message, in KiB.
fn kib_per_message(main_db_bytes: u64, messages: i64) -> f64 {
    if messages <= 0 {
        return 0.0;
    }
    main_db_bytes as f64 / 1024.0 / messages as f64
}

/// Convert `ru_maxrss` to bytes: macOS reports bytes, Linux kibibytes.
fn maxrss_bytes(raw: i64) -> u64 {
    let raw = raw.max(0) as u64;
    if cfg!(target_os = "macos") {
        raw
    } else {
        raw * 1024
    }
}

/// This process's peak resident set size so far, in bytes.
fn peak_rss_bytes() -> u64 {
    // SAFETY: `rusage` is plain old data, zero-initialised is a valid value,
    // and getrusage only writes into it.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    if rc != 0 {
        return 0;
    }
    maxrss_bytes(usage.ru_maxrss as i64)
}

fn copy_dir_recursive(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create scratch dir");
    for entry in std::fs::read_dir(src).expect("read profile dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        let to = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_recursive(&path, &to);
        } else {
            std::fs::copy(&path, &to).expect("copy profile file");
        }
    }
}

/// Removes the scratch copy on every exit path, panics included.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn main() {
    let src = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: perf_footprint <seeded-profile-dir>"),
    );
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(src.join("perf-profile.json")).expect("read manifest"),
    )
    .expect("parse manifest");
    let messages = manifest["messages"].as_i64().expect("manifest.messages");
    let mode = manifest["mode"].as_str().unwrap_or("descriptor-only");

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let scratch = Scratch(std::env::temp_dir().join(format!(
        "castellum-perf-footprint-{}-{stamp}",
        std::process::id()
    )));
    copy_dir_recursive(&src, &scratch.0);
    println!(
        "perf_footprint: {messages} messages, mode {mode}, profile {}",
        src.display()
    );

    let mut rows: Vec<String> = Vec::new();

    // On-disk rows: seeder-measured (see the header).
    match (
        manifest["main_db_bytes"].as_u64(),
        manifest["wal_bytes_after_seed"].as_u64(),
        manifest["wal_bytes_after_checkpoint"].as_u64(),
    ) {
        (Some(db), Some(wal_seed), Some(wal_ckpt)) => {
            rows.push(perf_line(
                &advisory_row("footprint_db_file_size", "MiB"),
                mib(db),
            ));
            rows.push(perf_line(
                &advisory_row("footprint_wal_after_seed", "MiB"),
                mib(wal_seed),
            ));
            rows.push(perf_line(
                &advisory_row("footprint_wal_after_checkpoint", "MiB"),
                mib(wal_ckpt),
            ));
            rows.push(perf_line(
                &advisory_row("footprint_bytes_per_message", "KiB/message"),
                kib_per_message(db, messages),
            ));
        }
        _ => println!(
            "INFO on-disk rows skipped: manifest has no footprint fields \
             (profile seeded before M2; re-seed it)"
        ),
    }
    if let Some(body_bytes) = manifest["body_bytes"].as_u64() {
        println!(
            "INFO body_bytes={} ({:.1} MiB of synthetic body text over {} bodies)",
            body_bytes,
            mib(body_bytes),
            manifest["bodies"].as_i64().unwrap_or(0)
        );
    }

    let baseline = peak_rss_bytes();
    println!(
        "INFO rss_before_open={:.1}MiB (process baseline)",
        mib(baseline)
    );

    // Memory rows: a fresh process opens the profile and runs the same reads
    // the gate times. Cumulative peaks, one row per stage.
    let layout = corpus_layout(messages);
    let acct_a = &layout[0];
    let inbox_id = format!("{}:{}", acct_a.id, acct_a.mailboxes[0].path);

    let profile = open_profile(&scratch.0).expect("open_profile");
    rows.push(perf_line(
        &advisory_row("footprint_peak_rss_open", "MiB"),
        mib(peak_rss_bytes()),
    ));

    // Launch reads + list paging: what `bootstrap()` and the first screen do.
    let _ = profile.list_accounts_json().expect("list_accounts_json");
    for account in &layout {
        let _ = profile.list_mailboxes_json(account.id).expect("mailboxes");
        let _ = profile.mailbox_counts_json(account.id).expect("counts");
    }
    let _ = profile
        .list_messages_by_role_page_json("inbox", None, 500)
        .expect("unified first page");
    let mut cursor: Option<String> = None;
    for _ in 0..4 {
        let json = profile
            .list_messages_page_json(acct_a.id, &inbox_id, cursor.as_deref(), 500)
            .expect("list page");
        let page: serde_json::Value = serde_json::from_str(&json).expect("parse page");
        cursor = (!page["next"].is_null())
            .then(|| serde_json::to_string(&page["next"]).expect("serialize cursor"));
    }
    rows.push(perf_line(
        &advisory_row("footprint_peak_rss_launch_list", "MiB"),
        mib(peak_rss_bytes()),
    ));

    // Search: the four shapes (common term, rare term, from:, structured).
    // The rare word is the first pool word that has a hit, as in the gate.
    let common = perf_common::most_common_word();
    let rare = (0..perf_common::WORD_POOL_SIZE)
        .map(perf_common::pool_word)
        .find(|w| {
            let json = profile
                .search_messages_across_accounts_json(w, 500)
                .expect("probe rare word");
            json != "[]"
        })
        .expect("some pool word occurs");
    for query in [
        common.clone(),
        rare,
        format!("from:@domain0.test {common}"),
        "is:unread has:attachment".to_string(),
    ] {
        let _ = profile
            .search_messages_across_accounts_json(&query, 500)
            .expect("search");
    }
    rows.push(perf_line(
        &advisory_row("footprint_peak_rss_search", "MiB"),
        mib(peak_rss_bytes()),
    ));

    // Counts rows: per-account mailbox counts and the smart-mailbox recount.
    for account in &layout {
        let _ = profile.mailbox_counts_json(account.id).expect("counts");
    }
    let _ = profile
        .recompute_all_smart_mailbox_counts_json()
        .expect("smart mailbox recount");
    rows.push(perf_line(
        &advisory_row("footprint_peak_rss_counts", "MiB"),
        mib(peak_rss_bytes()),
    ));
    drop(profile);

    println!();
    for row in &rows {
        println!("{row}");
    }
    println!("\nPASS: footprint rows are advisory (no target, no ceiling).");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advisory_rows_can_never_warn_or_fail() {
        let b = advisory_row("footprint_db_file_size", "MiB");
        assert_eq!(perf_common::judge(&b, 1.0e12), perf_common::Verdict::Pass);
    }

    #[test]
    fn unit_conversions() {
        assert_eq!(mib(1024 * 1024), 1.0);
        assert_eq!(kib_per_message(2048 * 10, 10), 2.0);
        assert_eq!(kib_per_message(5, 0), 0.0);
        let one_mib_raw = 1024 * 1024;
        let expected = if cfg!(target_os = "macos") {
            one_mib_raw
        } else {
            one_mib_raw * 1024
        };
        assert_eq!(maxrss_bytes(one_mib_raw as i64), expected);
        assert_eq!(maxrss_bytes(-1), 0);
    }

    #[test]
    fn this_process_has_a_nonzero_peak_rss() {
        assert!(peak_rss_bytes() > 1024 * 1024);
    }

    #[test]
    fn jsonl_row_carries_the_unit_and_null_budgets() {
        let b = advisory_row("footprint_bytes_per_message", "KiB/message");
        let row = perf_common::jsonl_row(
            &b,
            5.0,
            &perf_common::Verdict::Pass,
            "sha",
            "local",
            None,
            "2026-10-01T00:00:00Z",
        );
        let v: serde_json::Value = serde_json::from_str(&row).expect("json");
        assert_eq!(v["unit"], "KiB/message");
        assert!(v["target"].is_null() && v["ceiling"].is_null());
    }
}
