// Castellum - CI sync performance regression gate (mock provider, no network).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Drives the REAL `sync::sync_with_provider` (the same core loop
// `Profile::sync_imap_json` calls, see `examples/sync_bench.rs`) against a
// synthetic, in-memory `MailProvider` instead of GreenMail or a live IMAP
// account, so it can run in CI with no Docker and no network — exactly the
// approach `project-docs/perf/2026-09-sync-baseline.md`'s "Signed-off budgets
// for #2266" section calls for: "For CI, use a mock-provider harness (feed
// synthetic descriptors straight into `sync_with_provider`). GreenMail's
// server-side slowdown at large sizes would swamp any budget."
//
// Checks every budget in that section's final table (confirmed by Marc,
// 2026-09-23) against a synthetic 10k- and 100k-message corpus, including the
// no-O(n^2) gate: this is the exact shape of check that would have caught the
// `message_fts` full-scan bug PR #142 fixed, BEFORE it shipped — descriptor
// persist cost must stay flat as the corpus grows, not grow with it.
//
// BLOCKING vs ADVISORY (, decided 2026-09-23): the first CI run
// of this gate reproduced, on the real macbook-runner, the same three-budget
// failure pattern seen in local runs under machine load — `rebuild_threads`
// and everything downstream of its single large transaction (UI-read latency
// during sync, no-op resync time) read 1.5-2.5x their confirmed budget purely
// from scheduler/fsync contention with Marc's other concurrent Claude Code
// sessions on the same machine, not from a real regression, while the
// no-O(n^2) ratio and per-message/per-100 descriptor-persist budgets — the
// gate's actual regression-catching purpose — stayed reliably within budget
// throughout. Those three are therefore advisory (measured, printed, never
// fail the job); see `Check::blocking`'s doc comment for the full reasoning
// and which checks are which.
//
// Usage: cargo run --release -p castellum-core --example sync_perf_gate
// Exit code 0 = every BLOCKING budget met (also prints a full measured-vs-
// budget table either way, so a CI log is legible without re-running
// locally; an advisory budget can be over without changing the exit code).
// Exit code 1 = at least one BLOCKING budget exceeded; the same table marks
// which, blocking or advisory.
//
// No message content anywhere: descriptors are synthetic from the start, and
// only counts, byte totals and timings are ever printed — matching the
// discipline the baseline doc itself documents for its Gmail spot check.

#[path = "perf_common/mod.rs"]
mod perf_common;

use castellum_core::sync::{sync_with_provider, DESCRIPTOR_UPSERT_BATCH_SIZE};
use castellum_core::sync_timing::{self, SyncTimings};
use castellum_db::Database;
use perf_common::{pct, SyntheticProvider};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ACCOUNT_ID: &str = "perf-gate-acc";
const SMALL_N: i64 = 10_000;
const LARGE_N: i64 = 100_000;

// --- budgets (project-docs/perf/2026-09-sync-baseline.md, "Signed-off budgets
// for #2266", confirmed by Marc 2026-09-23 (profile size raised to 2.0 KiB 2026-10-05) — do not tune these without a doc
// update) -------------------------------------------------------------------

/// descriptor persist, per message p50 (@100k). Doc: "≤ 0.3 ms".
const DESCRIPTOR_P50_BUDGET_MS: f64 = 0.3;
/// no-O(n^2) gate: per-message p50 cost @100k / @10k. Doc: "≤ 1.5".
const NO_QUADRATIC_RATIO_BUDGET: f64 = 1.5;
/// descriptor persist, per 100 messages p95 (@100k). Doc: "≤ 50 ms".
const DESCRIPTOR_PER100_P95_BUDGET_MS: f64 = 50.0;
/// rebuild_threads (@100k). Doc: "≤ 1.0 s".
const REBUILD_THREADS_BUDGET_MS: f64 = 1_000.0;
/// UI read (500-row list + counts) during sync, p95 (@100k). Doc: "p95 ≤ 25 ms".
const UI_READ_P95_BUDGET_MS: f64 = 25.0;
/// UI read (500-row list + counts) during sync, max (@100k). Doc: "max ≤ 1 s".
const UI_READ_MAX_BUDGET_MS: f64 = 1_000.0;
/// no-op resync, DB/core time (@100k). Doc: "≤ 1 s @100k".
const NOOP_RESYNC_DB_CORE_BUDGET_MS: f64 = 1_000.0;
/// profile size, descriptor-only, per message (@100k). Doc: "≤ 2.0 KiB/message".
const PROFILE_SIZE_PER_MSG_BUDGET_KIB: f64 = 2.0;

// ---: small follow-up sync on an already-built 100k account
// (the incremental thread rebuild's actual target -- distinct from the
// "first sync" and "no-op resync" scenarios above, neither of which
// exercises it: a first sync has 100% of messages dirty by construction
// (correctly falls back to a full rebuild), and a no-op resync skips the
// rebuild entirely). See project-docs/perf/read-path-budgets.md's
// "Follow-up sync" section for the measured numbers these were set from
// and the load average at the time -- PROPOSED, not yet Marc-confirmed,
// same status as every other budget in that doc. ------------------------

/// New messages delivered per follow-up iteration.
const FOLLOWUP_NEW_MESSAGES: i64 = 20;
/// Existing messages whose \Seen flag flips per follow-up iteration (a
/// realistic "a few flags changed elsewhere" alongside the new mail).
const FOLLOWUP_FLAG_FLIPS: usize = 5;
/// Iterations, each delivering another `FOLLOWUP_NEW_MESSAGES` on top of the
/// previous ones, so `rebuild_threads_incremental`'s dirty set/closure size
/// stays representative of a real "check for new mail" cadence rather than
/// one lucky sample -- p50/p95/max are taken across these.
const FOLLOWUP_ITERATIONS: usize = 10;
/// `db.rebuild_threads` phase, follow-up sync, p95. Set from this stage's
/// own measurement -- three runs at load averages ~15-24 read p95
/// 45.0/46.2/46.7ms, p50 43.1-43.4ms (see project-docs/perf/
/// read-path-budgets.md's "Follow-up sync" section) -- rounded up a little
/// for headroom on a calmer machine than this one turned out to be.
const FOLLOWUP_REBUILD_THREADS_P95_TARGET_MS: f64 = 50.0;
/// BLOCKING: ~3x the measured p95 above ( CI-gate follow-up
/// task) -- comfortably above ordinary machine-load noise but WAY under the
/// ~1.5-3.5s a full rebuild costs at this corpus size (see the
/// `rebuild_threads (@100k)` row above), so a regression that falls back to
/// (or otherwise behaves like) a full rebuild fails this job.
const FOLLOWUP_REBUILD_THREADS_P95_CEILING_MS: f64 = 150.0;
/// Whole follow-up sync wall time, p95 (descriptor persist + full-scan flag
/// reconciliation over 100k+ uids + the rebuild phase above) -- advisory,
/// like the other whole-pass numbers in this gate ('s
/// load-sensitivity reasoning applies here too, since this includes the
/// same full-scan reconciliation); the rebuild-phase row above is the one
/// that actually isolates this change and is the one that blocks. Measured
/// p95 111.5-115.4ms across the same three runs.
const FOLLOWUP_SYNC_WALL_P95_TARGET_MS: f64 = 130.0;
const FOLLOWUP_SYNC_WALL_P95_CEILING_MS: f64 = 350.0;

// --- stats helpers (mirrors examples/sync_bench.rs) --------------------------

fn ms(us: u64) -> f64 {
    us as f64 / 1000.0
}

fn sorted_samples(t: &SyncTimings, phase: &str) -> Vec<u64> {
    let mut s = t.samples.get(phase).cloned().unwrap_or_default();
    s.sort_unstable();
    s
}

/// Per-message cost of descriptor persistence, sorted (in us). Since the tracker
/// #2503, `db.upsert_descriptor` records ONE sample per
/// `DESCRIPTOR_UPSERT_BATCH_SIZE`-message transaction, not one per message —
/// divide each batch's total by its message count to recover a per-message
/// estimate. Every batch here is the same size (both corpora are exact
/// multiples of the batch size), so a percentile over these batch-derived
/// values equals the same percentile over the equivalent per-message list
/// (each batch's estimate repeated `DESCRIPTOR_UPSERT_BATCH_SIZE` times) —
/// uniform replication doesn't move a percentile's relative position.
fn per_message_us(t: &SyncTimings, phase: &str) -> Vec<u64> {
    let mut s: Vec<u64> = t
        .samples
        .get(phase)
        .map(|v| {
            v.iter()
                .map(|&batch_total| batch_total / DESCRIPTOR_UPSERT_BATCH_SIZE as u64)
                .collect()
        })
        .unwrap_or_default();
    s.sort_unstable();
    s
}

/// Per-100-message cost of descriptor persistence, sorted (in us), derived
/// from the per-`DESCRIPTOR_UPSERT_BATCH_SIZE`-message batch samples
/// `db.upsert_descriptor` now records by linear scaling
/// (assumes uniform per-message cost within a batch, matching how the old
/// per-100 metric assumed uniform cost across its own 100-message chunks).
fn per_100_batches_us(t: &SyncTimings) -> Vec<u64> {
    let ups = t
        .samples
        .get("db.upsert_descriptor")
        .cloned()
        .unwrap_or_default();
    let mut per100: Vec<u64> = ups
        .iter()
        .map(|&batch_total| batch_total * 100 / DESCRIPTOR_UPSERT_BATCH_SIZE as u64)
        .collect();
    per100.sort_unstable();
    per100
}

/// Sum of every `db.*`/`core.*` phase — the DB/core half of the split
/// `sync_bench.rs` prints (excludes `imap.*`, i.e. provider round trips,
/// which are zero-cost here anyway since the provider is in-memory).
fn db_core_us(t: &SyncTimings) -> u64 {
    t.samples
        .iter()
        .filter(|(k, _)| k.starts_with("db.") || k.starts_with("core."))
        .map(|(_, v)| v.iter().sum::<u64>())
        .sum()
}

fn dir_size(p: &Path) -> u64 {
    std::fs::read_dir(p)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.metadata().map_or(0, |m| m.len()))
                .sum()
        })
        .unwrap_or(0)
}

// --- concurrent UI-read probe (mirrors examples/sync_bench.rs) ---------------

fn spawn_reader(
    db: Arc<Database>,
    account_id: String,
    mailbox_id: String,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<Vec<u64>> {
    std::thread::spawn(move || {
        let mut lat = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            let t = Instant::now();
            let _ = db.list_message_headers(&account_id, &mailbox_id, 500, 0);
            let _ = db.mailbox_counts(&account_id);
            lat.push(t.elapsed().as_micros() as u64);
            std::thread::sleep(Duration::from_millis(50));
        }
        lat
    })
}

// --- corpus runs --------------------------------------------------------------

struct CorpusRun {
    timings: SyncTimings,
    reader_latencies_us: Option<Vec<u64>>,
}

/// Run a from-scratch synthetic sync of `n` messages into a fresh profile
/// under `dir`. When `with_reader` is set, a concurrent thread hammers
/// `list_message_headers`/`mailbox_counts` (the "UI read" probe) for the
/// duration of the sync. Returns the opened `Database` (kept alive so the
/// caller can run a no-op resync pass against the same profile) and the
/// recorded timings.
fn run_first_sync(dir: &Path, n: i64, with_reader: bool) -> (Arc<Database>, CorpusRun, Duration) {
    let db = Arc::new(Database::open(&dir.join("castellum.db")).expect("open synthetic profile"));
    let mailbox_id = format!("{ACCOUNT_ID}:INBOX");

    let stop = Arc::new(AtomicBool::new(false));
    let reader_handle = with_reader.then(|| {
        spawn_reader(
            Arc::clone(&db),
            ACCOUNT_ID.to_string(),
            mailbox_id.clone(),
            Arc::clone(&stop),
        )
    });

    let mut provider = SyntheticProvider::single_inbox(n);
    sync_timing::enable();
    let started = Instant::now();
    let report = sync_with_provider(
        &db,
        ACCOUNT_ID,
        "perf-gate@synthetic.test",
        None,
        &mut provider,
        "synthetic-host",
    )
    .expect("sync_with_provider (synthetic first sync)");
    let wall = started.elapsed();
    let timings = sync_timing::take().expect("sync_timing was enabled for this call");

    stop.store(true, Ordering::Relaxed);
    let reader_latencies_us = reader_handle.map(|h| h.join().expect("reader thread"));

    let fetched: i64 = report.mailboxes.iter().map(|m| m.fetched as i64).sum();
    assert_eq!(
        fetched, n,
        "synthetic provider must deliver every descriptor on a clean first sync"
    );

    (
        db,
        CorpusRun {
            timings,
            reader_latencies_us,
        },
        wall,
    )
}

/// Re-sync the same `n`-message corpus against an unchanged provider: the
/// cursor already covers every UID, so this must be a genuine no-op (0
/// fetched) — exactly what happens when the UI polls/resyncs an idle account.
fn run_noop_resync(db: &Database, n: i64) -> SyncTimings {
    let mut provider = SyntheticProvider::single_inbox(n);
    sync_timing::enable();
    let report = sync_with_provider(
        db,
        ACCOUNT_ID,
        "perf-gate@synthetic.test",
        None,
        &mut provider,
        "synthetic-host",
    )
    .expect("sync_with_provider (no-op resync)");
    let timings = sync_timing::take().expect("sync_timing was enabled for this call");
    assert_eq!(
        report.mailboxes[0].fetched, 0,
        "resync against an unchanged synthetic corpus must fetch nothing"
    );
    timings
}

/// One follow-up-sync iteration: the account already has
/// `n_before` messages and a completed cursor; this delivers
/// `FOLLOWUP_NEW_MESSAGES` brand-new ones plus `FOLLOWUP_FLAG_FLIPS`
/// existing-message flag flips (alternating so each call is a genuine
/// change, never a no-op), times the whole sync AND isolates the
/// `db.rebuild_threads` phase specifically, and returns the new total
/// message count for the next iteration to build on.
fn run_followup_sync_iteration(
    db: &Database,
    n_before: i64,
    flip_uids: &[i64],
    iteration: usize,
) -> (Duration, u64, i64) {
    let flip_seen_to: Vec<(i64, bool)> = flip_uids
        .iter()
        .map(|&uid| (uid, iteration.is_multiple_of(2)))
        .collect();
    let mut provider = SyntheticProvider::single_inbox_with_followup(
        n_before,
        FOLLOWUP_NEW_MESSAGES,
        &flip_seen_to,
    );
    sync_timing::enable();
    let started = Instant::now();
    let report = sync_with_provider(
        db,
        ACCOUNT_ID,
        "perf-gate@synthetic.test",
        None,
        &mut provider,
        "synthetic-host",
    )
    .expect("sync_with_provider (follow-up sync)");
    let wall = started.elapsed();
    let timings = sync_timing::take().expect("sync_timing was enabled for this call");
    let fetched: i64 = report.mailboxes.iter().map(|m| m.fetched as i64).sum();
    assert_eq!(
        fetched, FOLLOWUP_NEW_MESSAGES,
        "a follow-up sync must fetch exactly the new messages this iteration delivered, \
         no more and no less -- a mismatch means the scenario isn't measuring what its \
         name says"
    );
    let rebuild_threads_us = timings.total_us("db.rebuild_threads");
    (wall, rebuild_threads_us, n_before + FOLLOWUP_NEW_MESSAGES)
}

// --- budget checks -------------------------------------------------------------

struct Check {
    name: &'static str,
    measured: String,
    budget: String,
    pass: bool,
    /// (2026-09-23, Marc's decision): only the no-O(n^2) ratio
    /// and per-message/per-100 descriptor-persist budgets gate CI. The three
    /// single-large-transaction budgets (`rebuild_threads` and everything
    /// downstream of it: UI-read latency during sync, no-op resync time) are
    /// measured and printed but never fail the job — on `macbook-runner`
    /// (Marc's own MacBook, routinely running several concurrent interactive
    /// Claude Code sessions, see the `tmux-estate` skill) those three read
    /// 1.5-2.5x their confirmed budget purely from scheduler/fsync
    /// contention, reproduced identically on three separate runs (two local,
    /// one on the real macbook-runner CI job) at similar magnitudes, while
    /// the blocking checks stayed reliably within budget throughout. Making
    /// them blocking would turn `perf-gate` red on ordinary machine load, not
    /// just on a real regression — exactly the false-positive-desensitizes-
    /// everyone failure mode a CI gate must avoid. profile size stays
    /// blocking: it is a single read of on-disk bytes, not contention-prone.
    blocking: bool,
}

fn check(
    name: &'static str,
    measured: f64,
    budget: f64,
    unit: &'static str,
    pass: bool,
    blocking: bool,
) -> Check {
    // Trend JSONL: a no-op unless CASTELLUM_PERF_JSONL is set.
    // This gate has one budget per row, so target == ceiling.
    let verdict = match (pass, blocking) {
        (true, _) => perf_common::Verdict::Pass,
        (false, true) => perf_common::Verdict::Fail,
        (false, false) => perf_common::Verdict::Warn,
    };
    perf_common::record_row(
        &perf_common::Budget {
            name,
            unit,
            target: budget,
            ceiling: budget,
        },
        measured,
        &verdict,
    );
    Check {
        name,
        measured: format!("{measured:.3} {unit}"),
        budget: format!("<= {budget:.3} {unit}"),
        pass,
        blocking,
    }
}

fn print_report(checks: &[Check]) {
    println!(
        "\n{:<62} {:>16} {:>16} {:>10}",
        "check", "measured", "budget", ""
    );
    println!("{}", "-".repeat(108));
    for c in checks {
        let status = match (c.pass, c.blocking) {
            (true, _) => "PASS",
            (false, true) => "FAIL",
            // Measured, over budget, but does not fail the job -- see
            // `Check::blocking`'s doc comment.
            (false, false) => "WARN(adv)",
        };
        println!(
            "{:<62} {:>16} {:>16} {:>10}",
            c.name, c.measured, c.budget, status
        );
    }
}

fn main() {
    println!("Castellum sync perf gate -- mock-provider CI regression gate");
    println!(
        "Corpus: {SMALL_N} and {LARGE_N} synthetic messages, single INBOX, no network/GreenMail."
    );

    let gen_started = Instant::now();
    let probe_account = &perf_common::corpus_layout(1_000)[0];
    let probe_mailbox = probe_account.mailboxes[0];
    let mut probe_rng = perf_common::Rng::new(2266);
    let probe = perf_common::synthetic_descriptors(&mut probe_rng, probe_account, &probe_mailbox);
    println!(
        "(descriptor generation: {:.3} ms per 1,000 synthetic descriptors, extrapolated)",
        gen_started.elapsed().as_secs_f64() * 1000.0
    );
    drop(probe);

    // --- 10k corpus: only the no-O(n^2) ratio needs this size ----------------
    let dir_small = tempfile::tempdir().expect("tempdir");
    let small_started = Instant::now();
    let (_db_small, small, small_wall) = run_first_sync(dir_small.path(), SMALL_N, false);
    println!(
        "10k first sync: wall {:.2} s (harness total {:.2} s incl. profile open)",
        small_wall.as_secs_f64(),
        small_started.elapsed().as_secs_f64()
    );
    let small_p50_us = pct(&per_message_us(&small.timings, "db.upsert_descriptor"), 0.5);

    // --- 100k corpus: every other budget is measured here ---------------------
    let dir_large = tempfile::tempdir().expect("tempdir");
    let large_started = Instant::now();
    let (db_large, large, large_wall) = run_first_sync(dir_large.path(), LARGE_N, true);
    println!(
        "100k first sync: wall {:.2} s (harness total {:.2} s incl. profile open)",
        large_wall.as_secs_f64(),
        large_started.elapsed().as_secs_f64()
    );
    let large_upsert_batches = sorted_samples(&large.timings, "db.upsert_descriptor");
    let large_p50_us = pct(&per_message_us(&large.timings, "db.upsert_descriptor"), 0.5);
    // One `db.upsert_descriptor` sample per transaction since
    // (DESCRIPTOR_UPSERT_BATCH_SIZE messages each) rather than one per
    // message -- assert on batch count, and that they cover every message
    // (both corpora are exact multiples of the batch size).
    assert_eq!(
        large_upsert_batches.len() * DESCRIPTOR_UPSERT_BATCH_SIZE,
        LARGE_N as usize,
        "every synthetic message must have persisted, in full batches"
    );

    // Measure on-disk footprint from CHECKPOINTED state, not whatever happens
    // to still be sitting in the WAL file at the instant the sync call
    // returns (SQLite auto-checkpoints opportunistically, and the concurrent
    // UI-read probe above can hold the WAL back from checkpointing while it
    // runs) — otherwise this budget measures WAL-flush timing, not descriptor
    // storage cost. Dropping the last handle closes the connection, which
    // performs SQLite's own close-time checkpoint; reopening (idempotent, see
    // `Database::open`'s doc comment) costs nothing extra before the no-op
    // resync pass that follows.
    drop(db_large);
    let profile_bytes = dir_size(dir_large.path());
    let db_large =
        Database::open(&dir_large.path().join("castellum.db")).expect("reopen synthetic profile");

    let resync_started = Instant::now();
    let resync_timings = run_noop_resync(&db_large, LARGE_N);
    println!(
        "100k no-op resync: harness total {:.2} s",
        resync_started.elapsed().as_secs_f64()
    );

    // ---: small follow-up sync on the SAME already-built
    // 100k account, repeated FOLLOWUP_ITERATIONS times so the dirty-set/
    // closure size stays representative rather than one lucky sample. The
    // 5 flipped uids are fixed (scattered across the corpus, none of them
    // touching each other or the newly-inserted messages -- this synthetic
    // corpus has no References chains at all, so every closure is exactly
    // its own dirty set) and alternate is_seen every iteration so each one
    // is a genuine reconciled change, not a no-op after the first.
    let flip_uids: [i64; FOLLOWUP_FLAG_FLIPS] = [17, 5_000, 45_000, 70_000, 99_999];
    let mut followup_n = LARGE_N;
    let mut followup_wall_us: Vec<u64> = Vec::with_capacity(FOLLOWUP_ITERATIONS);
    let mut followup_rebuild_us: Vec<u64> = Vec::with_capacity(FOLLOWUP_ITERATIONS);
    let followup_started = Instant::now();
    for i in 0..FOLLOWUP_ITERATIONS {
        let (wall, rebuild_us, next_n) =
            run_followup_sync_iteration(&db_large, followup_n, &flip_uids, i);
        followup_wall_us.push(wall.as_micros() as u64);
        followup_rebuild_us.push(rebuild_us);
        followup_n = next_n;
    }
    followup_wall_us.sort_unstable();
    followup_rebuild_us.sort_unstable();
    println!(
        "100k follow-up sync: {FOLLOWUP_ITERATIONS} iterations x {FOLLOWUP_NEW_MESSAGES} new \
         messages + {FOLLOWUP_FLAG_FLIPS} flag flips, harness total {:.2} s",
        followup_started.elapsed().as_secs_f64()
    );

    // --- assemble checks --------------------------------------------------------
    let mut checks = Vec::new();

    let descriptor_p50_ms = ms(large_p50_us);
    checks.push(check(
        "descriptor persist, per message p50 (@100k)",
        descriptor_p50_ms,
        DESCRIPTOR_P50_BUDGET_MS,
        "ms",
        descriptor_p50_ms <= DESCRIPTOR_P50_BUDGET_MS,
        true,
    ));

    let quadratic_ratio = if small_p50_us == 0 {
        // Sub-microsecond-resolution corpus: cannot regress into O(n^2) from
        // an unmeasurably small baseline. Treat as a pass rather than a
        // divide-by-zero, and say so via the ratio value itself (NaN would
        // print badly; report 1.0, the "flat" baseline the gate wants).
        1.0
    } else {
        large_p50_us as f64 / small_p50_us as f64
    };
    checks.push(check(
        "no-O(n^2) gate: per-msg p50 cost @100k / @10k",
        quadratic_ratio,
        NO_QUADRATIC_RATIO_BUDGET,
        "x",
        quadratic_ratio <= NO_QUADRATIC_RATIO_BUDGET,
        true,
    ));

    let per100_p95_ms = ms(pct(&per_100_batches_us(&large.timings), 0.95));
    checks.push(check(
        "descriptor persist, per 100 messages p95 (@100k)",
        per100_p95_ms,
        DESCRIPTOR_PER100_P95_BUDGET_MS,
        "ms",
        per100_p95_ms <= DESCRIPTOR_PER100_P95_BUDGET_MS,
        true,
    ));

    // Advisory only: downstream of a single large
    // transaction, and reproducibly 1.5-2.5x over budget under this
    // machine's routine concurrent-session load rather than on a real
    // regression. See `Check::blocking`'s doc comment.
    let rebuild_threads_ms = ms(large.timings.total_us("db.rebuild_threads"));
    checks.push(check(
        "rebuild_threads (@100k)",
        rebuild_threads_ms,
        REBUILD_THREADS_BUDGET_MS,
        "ms",
        rebuild_threads_ms <= REBUILD_THREADS_BUDGET_MS,
        false,
    ));

    let reader_lat = large.reader_latencies_us.clone().unwrap_or_default();
    let mut reader_sorted = reader_lat.clone();
    reader_sorted.sort_unstable();
    let ui_p95_ms = ms(pct(&reader_sorted, 0.95));
    let ui_max_ms = ms(*reader_sorted.last().unwrap_or(&0));
    checks.push(check(
        "UI read (500-row list + counts) during sync, p95 (@100k)",
        ui_p95_ms,
        UI_READ_P95_BUDGET_MS,
        "ms",
        ui_p95_ms <= UI_READ_P95_BUDGET_MS,
        false,
    ));
    checks.push(check(
        "UI read (500-row list + counts) during sync, max (@100k)",
        ui_max_ms,
        UI_READ_MAX_BUDGET_MS,
        "ms",
        ui_max_ms <= UI_READ_MAX_BUDGET_MS,
        false,
    ));

    let resync_db_core_ms = ms(db_core_us(&resync_timings));
    checks.push(check(
        "no-op resync, DB/core time (@100k)",
        resync_db_core_ms,
        NOOP_RESYNC_DB_CORE_BUDGET_MS,
        "ms",
        resync_db_core_ms <= NOOP_RESYNC_DB_CORE_BUDGET_MS,
        false,
    ));

    let kib_per_msg = (profile_bytes as f64 / 1024.0) / LARGE_N as f64;
    checks.push(check(
        "profile size, descriptor-only, per message (@100k)",
        kib_per_msg,
        PROFILE_SIZE_PER_MSG_BUDGET_KIB,
        "KiB",
        kib_per_msg <= PROFILE_SIZE_PER_MSG_BUDGET_KIB,
        true,
    ));

    //: the scenario the incremental thread rebuild actually
    // targets -- see the constants' doc comments above for why neither the
    // first-sync nor the no-op-resync checks above exercise it. Two rows per
    // metric (advisory target, blocking ceiling), matching this file's
    // existing single-budget `Check` shape rather than read_perf_gate's
    // combined target/ceiling line.
    let followup_rebuild_p95_ms = ms(pct(&followup_rebuild_us, 0.95));
    let followup_rebuild_max_ms = ms(*followup_rebuild_us.last().unwrap_or(&0));
    println!(
        "\nINFO followup_sync db.rebuild_threads p50={:.3}ms p95={:.3}ms max={:.3}ms",
        ms(pct(&followup_rebuild_us, 0.5)),
        followup_rebuild_p95_ms,
        followup_rebuild_max_ms
    );
    checks.push(check(
        "follow-up sync, db.rebuild_threads phase, p95 (target, @100k+20)",
        followup_rebuild_p95_ms,
        FOLLOWUP_REBUILD_THREADS_P95_TARGET_MS,
        "ms",
        followup_rebuild_p95_ms <= FOLLOWUP_REBUILD_THREADS_P95_TARGET_MS,
        false,
    ));
    checks.push(check(
        "follow-up sync, db.rebuild_threads phase, p95 (ceiling, @100k+20)",
        followup_rebuild_p95_ms,
        FOLLOWUP_REBUILD_THREADS_P95_CEILING_MS,
        "ms",
        followup_rebuild_p95_ms <= FOLLOWUP_REBUILD_THREADS_P95_CEILING_MS,
        true,
    ));

    let followup_wall_p95_ms = ms(pct(&followup_wall_us, 0.95));
    checks.push(check(
        "follow-up sync, whole pass, p95 (target, @100k+20)",
        followup_wall_p95_ms,
        FOLLOWUP_SYNC_WALL_P95_TARGET_MS,
        "ms",
        followup_wall_p95_ms <= FOLLOWUP_SYNC_WALL_P95_TARGET_MS,
        false,
    ));
    checks.push(check(
        "follow-up sync, whole pass, p95 (ceiling, @100k+20)",
        followup_wall_p95_ms,
        FOLLOWUP_SYNC_WALL_P95_CEILING_MS,
        "ms",
        followup_wall_p95_ms <= FOLLOWUP_SYNC_WALL_P95_CEILING_MS,
        false,
    ));

    print_report(&checks);

    println!(
        "\n(Not gated here -- manual only, per the baseline doc: real-account 80k-message Gmail \
         INBOX first sync <= 3 min end to end, progress every 5 s.)"
    );

    let advisory_over = checks.iter().any(|c| !c.pass && !c.blocking);
    if advisory_over {
        println!(
            "\nNOTE: one or more advisory (non-blocking) budgets were exceeded -- see WARN(adv) \
             rows above. These are downstream of a single large transaction and known to be \
             sensitive to concurrent load on this machine; investigate if this \
             persists on an otherwise-idle runner, but it does not fail this job."
        );
    }

    if checks.iter().any(|c| !c.pass && c.blocking) {
        if perf_common::advisory() {
            println!(
                "\nWARN(adv): blocking budgets exceeded, ignored (CASTELLUM_PERF_ADVISORY=1)."
            );
            return;
        }
        eprintln!("\nFAIL: one or more BLOCKING sync perf budgets exceeded (see table above).");
        std::process::exit(1);
    }
    println!("\nPASS: all blocking sync perf budgets within range.");
}
