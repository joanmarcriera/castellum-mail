// Castellum - CI read-path performance regression gate (S64).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Times the core calls the app makes at launch, while scrolling the mailbox
// and unified-inbox lists, and on search, plus the S30 QRESYNC no-op resync
// and the embeddings backlog -- against a 100k-message seeded profile
// (`seed_profile.rs`). PR #150's `sync_perf_gate` checks the *write* path;
// this is its read-path twin. Every budget prints one `perf_common::
// perf_line` (`PERF <name> measured=<v><unit> target=<v> ceiling=<v>
// <PASS|WARN|FAIL>`); exit 1 iff any budget is FAIL.
//
// Usage: cargo run --release -p castellum-core --example read_perf_gate \
//            -- [<seeded-profile-dir>]
// With no argument, seeds a fresh temp profile first (via the sibling
// `seed_profile` binary next to this one) so a laptop run is one command.
// With one, COPIES it to a temp dir first so measurements never mutate the
// fixture, and deletes that copy (and a self-seeded profile) on exit.
// Warm-up 3, then 20 iterations per budget; reports p95 (p50 alongside).
//
// Every row must time what its name claims, so the gate refuses to run a
// vacuous one: each search shape's hit count is checked against a band
// before any timing (`SearchShape`), the no-op resync asserts it took the
// QRESYNC path and measured > 0 ms, and the embed-queue row asserts the
// seeded backlog is there. Besides the 15 `PERF` lines it prints `SEARCH`
// (hits per shape) and `INFO` lines (the contacts-refresh share of the
// launch path, the full-scan no-op resync for comparison).
//
// No message content anywhere in stdout: only counts, byte totals,
// timings and the harness's own synthetic query terms (Global
// Constraints).

#[path = "perf_common/mod.rs"]
mod perf_common;

use castellum_core::sync::sync_with_provider;
use castellum_core::sync_timing;
use castellum_core::{open_profile, Profile};
use castellum_db::Database;
use perf_common::{
    corpus_layout, pct, perf_line, AccountSpec, Budget, Rng, SyntheticProvider, Verdict,
};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::time::Instant;

const WARMUP_ITERS: usize = 3;
const MEASURED_ITERS: usize = 20;

/// Rows each search asks for -- what the app asks for (`AppModel` caps
/// search results at 500), so a hit count reads as `min(matches, 500)`.
const SEARCH_LIMIT: i64 = 500;

/// A search the gate times, and the hit-count band that makes it the shape
/// its budget row claims. Checked BEFORE any timing: a query outside its
/// band is a harness bug (the corpus drifted, or a label no longer matches
/// the query it builds), and it aborts the run instead of timing something
/// else. Review round 3 found `search_common_term` and
/// `search_from_operator` timing zero-hit queries (0.03 ms, a guaranteed
/// PASS) because the "most common" word was really the rarest.
struct SearchShape {
    budget: &'static str,
    query: String,
    hits: RangeInclusive<usize>,
}

impl SearchShape {
    /// The band as printed: `>=100` for an open-ended floor, else `1..=20`.
    fn band(&self) -> String {
        if *self.hits.end() == usize::MAX {
            format!(">={}", self.hits.start())
        } else {
            format!("{}..={}", self.hits.start(), self.hits.end())
        }
    }

    fn check(&self, hits: usize) -> Result<(), String> {
        if self.hits.contains(&hits) {
            Ok(())
        } else {
            Err(format!(
                "{}: {hits} hits for its query, outside the {} band that makes it the \
                 shape its row claims -- refusing to time a query that is not what the \
                 budget says it is",
                self.budget,
                self.band()
            ))
        }
    }
}

/// How many rows `query` returns (capped at `SEARCH_LIMIT`).
fn search_hits(profile: &Profile, query: &str) -> usize {
    let json = profile
        .search_messages_across_accounts_json(query, SEARCH_LIMIT)
        .expect("search_messages_across_accounts_json");
    serde_json::from_str::<Vec<serde_json::Value>>(&json)
        .expect("search result is a JSON array")
        .len()
}

/// A rare term: a word in 1..=20 messages (the plan's "< 20"), never 0 --
/// a zero-hit lookup would time FTS's empty-posting-list shortcut instead.
const RARE_HITS: RangeInclusive<usize> = 1..=20;

/// The lowest-index pool word whose hit count lands in `RARE_HITS`, with
/// that count. Low indices are the rare end of `zipfish_word`'s draw, but
/// the very lowest are absent from the corpus entirely, so the gate scans
/// up to the first one that is present (deterministic for a given seed).
fn pick_rare_word(mut hits: impl FnMut(&str) -> usize) -> Option<(String, usize)> {
    (0..perf_common::WORD_POOL_SIZE)
        .map(perf_common::pool_word)
        .map(|word| {
            let n = hits(&word);
            (word, n)
        })
        .find(|(_, n)| RARE_HITS.contains(n))
}

/// The four search shapes, sized for `messages`. Floors scale with the
/// corpus (100 hits per 100k messages for the common and structured-only
/// shapes) so a smaller smoke profile still exercises them; the from:
/// shape needs at least one hit at any size.
fn search_shapes(profile: &Profile, messages: i64) -> Vec<SearchShape> {
    let floor = ((messages.max(0) as usize) / 1_000).max(1);
    let common_word = perf_common::most_common_word();
    let (rare_word, _) = pick_rare_word(|w| search_hits(profile, w)).unwrap_or_else(|| {
        panic!("no pool word has {RARE_HITS:?} hits in this profile ({messages} messages)")
    });
    vec![
        SearchShape {
            budget: "search_common_term",
            query: common_word.clone(),
            hits: floor..=usize::MAX,
        },
        SearchShape {
            budget: "search_rare_term",
            query: rare_word,
            hits: RARE_HITS,
        },
        SearchShape {
            // Senders are `sender{i}@domain{i % 50}.test`; `from:` is a
            // substring match on the address, so this is the 10 senders of
            // domain0 (2 % of the corpus) AND the common word.
            budget: "search_from_operator",
            query: format!("from:@domain0.test {common_word}"),
            hits: 1..=usize::MAX,
        },
        SearchShape {
            budget: "search_structured_only",
            query: "is:unread has:attachment".to_string(),
            hits: floor..=usize::MAX,
        },
    ]
}

fn budgets() -> Vec<Budget> {
    vec![
        Budget {
            name: "open_profile",
            unit: "ms",
            target: 150.0,
            ceiling: 600.0,
        },
        Budget {
            name: "launch_data_path",
            unit: "ms",
            target: 400.0,
            ceiling: 1_500.0,
        },
        Budget {
            name: "list_first_page",
            unit: "ms",
            target: 25.0,
            ceiling: 100.0,
        },
        Budget {
            name: "list_deep_page",
            unit: "ms",
            target: 25.0,
            ceiling: 100.0,
        },
        Budget {
            name: "list_depth_ratio",
            unit: "x",
            target: 2.0,
            ceiling: 2.0,
        },
        Budget {
            name: "unified_first_page",
            unit: "ms",
            target: 40.0,
            ceiling: 150.0,
        },
        Budget {
            name: "counts_all_accounts",
            unit: "ms",
            target: 20.0,
            ceiling: 80.0,
        },
        Budget {
            name: "smart_mailbox_recount",
            unit: "ms",
            target: 150.0,
            ceiling: 600.0,
        },
        // Search rows 9-12: each query is built by `search_shapes` and its
        // hit count checked against a band before it is timed.
        Budget {
            name: "search_common_term",
            unit: "ms",
            target: 100.0,
            ceiling: 400.0,
        },
        Budget {
            name: "search_rare_term",
            unit: "ms",
            target: 15.0,
            ceiling: 60.0,
        },
        Budget {
            name: "search_from_operator",
            unit: "ms",
            target: 100.0,
            ceiling: 400.0,
        },
        Budget {
            name: "search_structured_only",
            unit: "ms",
            target: 80.0,
            ceiling: 300.0,
        },
        // Wall time of the whole no-op resync (`noop_resync_ms`), not the
        // `db.*`/`core.*` phase sum it used to be -- none of those phases
        // run on a QRESYNC no-op, so that read 0.000 ms and could never fail.
        Budget {
            name: "qresync_noop_resync",
            unit: "ms",
            target: 50.0,
            ceiling: 250.0,
        },
        // Advisory only (Assumption 7): no ceiling that can ever fail the job.
        Budget {
            name: "embed_queue_next_job",
            unit: "ms",
            target: 5.0,
            ceiling: f64::INFINITY,
        },
        // Advisory only (#3633 G13): the un-hydrated-body count the app runs
        // beside every search (worst case = all accounts, no narrowing).
        Budget {
            name: "search_body_coverage_count",
            unit: "ms",
            target: 25.0,
            ceiling: f64::INFINITY,
        },
    ]
}

fn p50_p95(mut samples_ms: Vec<f64>) -> (f64, f64) {
    samples_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let sorted_us: Vec<u64> = samples_ms.iter().map(|ms| (*ms * 1000.0) as u64).collect();
    (
        pct(&sorted_us, 0.5) as f64 / 1000.0,
        pct(&sorted_us, 0.95) as f64 / 1000.0,
    )
}

/// Time `f`, warm-up 3 then `MEASURED_ITERS`, and return (p50, p95) in ms.
fn measure<F: FnMut()>(mut f: F) -> (f64, f64) {
    measure_value(|| {
        let started = Instant::now();
        f();
        started.elapsed().as_secs_f64() * 1000.0
    })
}

/// Like [`measure`], but `f` reports its own per-iteration cost in ms
/// instead of the closure's wall time -- used where each iteration has
/// untimed setup (`noop_resync_ms` rebuilds its provider first).
fn measure_value<F: FnMut() -> f64>(mut f: F) -> (f64, f64) {
    for _ in 0..WARMUP_ITERS {
        f();
    }
    let mut samples = Vec::with_capacity(MEASURED_ITERS);
    for _ in 0..MEASURED_ITERS {
        samples.push(f());
    }
    p50_p95(samples)
}

fn copy_dir_recursive(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create dst dir");
    for entry in std::fs::read_dir(src).expect("read src dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_recursive(&path, &dst_path);
        } else {
            std::fs::copy(&path, &dst_path).expect("copy file");
        }
    }
}

/// Directories this run created (the work copy, plus the seed when the gate
/// seeded its own): removed on every exit path. The work copy is written to
/// (migrations, the contacts refresh, a ~100 MB QRESYNC scratch DB), so
/// leaving it behind cost ~240 MB of disk per run -- earlier rounds of this
/// stage left 1.6 GB in $TMPDIR.
struct ScratchDirs(Vec<PathBuf>);

impl ScratchDirs {
    fn cleanup(&mut self) {
        for dir in self.0.drain(..) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Also on panic: a failed `expect` unwinds through this. (`process::exit`
/// skips destructors, so exit paths call `cleanup` first.)
impl Drop for ScratchDirs {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// Resolve the seeded profile directory: an explicit arg is copied to a
/// scratch dir first (never mutate the fixture); no arg seeds a fresh one
/// via the sibling `seed_profile` binary.
fn resolve_profile_dir(arg: Option<String>) -> (PathBuf, i64, ScratchDirs) {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let work_dir = std::env::temp_dir().join(format!(
        "castellum-read-perf-gate-{}-{stamp}",
        std::process::id()
    ));

    let mut scratch = ScratchDirs(vec![work_dir.clone()]);
    let source_dir = match arg {
        Some(dir) => PathBuf::from(dir),
        None => {
            let seed_dir = std::env::temp_dir().join(format!(
                "castellum-read-perf-gate-seed-{}-{stamp}",
                std::process::id()
            ));
            let seeder = std::env::current_exe()
                .expect("current_exe")
                .parent()
                .expect("examples dir")
                .join("seed_profile");
            let status = std::process::Command::new(&seeder)
                .arg(&seed_dir)
                .status()
                .unwrap_or_else(|e| panic!("spawn seed_profile ({}): {e}", seeder.display()));
            scratch.0.push(seed_dir.clone());
            assert!(status.success(), "seed_profile exited non-zero");
            seed_dir
        }
    };

    copy_dir_recursive(&source_dir, &work_dir);
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(work_dir.join("perf-profile.json")).expect("read manifest"),
    )
    .expect("parse manifest");
    let messages = manifest["messages"].as_i64().expect("manifest.messages");
    (work_dir, messages, scratch)
}

fn main() {
    let arg = std::env::args().nth(1);
    let (dir, messages, mut scratch) = resolve_profile_dir(arg);
    println!(
        "read_perf_gate: profile at {} ({messages} messages)",
        dir.display()
    );

    let layout = corpus_layout(messages);
    let acct_a = &layout[0];
    let acct_a_inbox_id = format!("{}:{}", acct_a.id, acct_a.mailboxes[0].path);

    let mut lines = Vec::new();
    let budgets = budgets();
    let b = |name: &str| budgets.iter().find(|b| b.name == name).expect("budget");

    // #1 open_profile -- fresh process-level open each iteration.
    let (open_p50, open_p95) = measure(|| {
        let profile = open_profile(&dir).expect("open_profile");
        drop(profile);
    });
    lines.push((perf_line(b("open_profile"), open_p95), open_p50, open_p95));

    // Keep one profile open for the launch-data-path / list / search /
    // counts budgets, mirroring a real launched app's single open.
    let profile = open_profile(&dir).expect("open_profile (shared)");

    // Validate every search shape before timing anything (see SearchShape).
    let shapes = search_shapes(&profile, messages);
    for shape in &shapes {
        let hits = search_hits(&profile, &shape.query);
        println!(
            "SEARCH {} hits={hits} band={} query=\"{}\"",
            shape.budget,
            shape.band(),
            shape.query
        );
        if let Err(e) = shape.check(hits) {
            eprintln!("\nFAIL: {e}");
            scratch.cleanup();
            std::process::exit(1);
        }
    }

    // #2 launch_data_path: what `AppModel.bootstrap()` reads before the first
    // list. M6: the contacts refresh is NOT in this row any more -- the app
    // runs it after the first list is delivered (it recomputes an aggregate
    // over every address row and dominated this row at 100k). It is still
    // timed on its own below (INFO line) so its cost stays visible.
    let mut contacts_ms = Vec::with_capacity(MEASURED_ITERS);
    let (launch_p50, launch_p95) = measure(|| {
        let _ = profile.list_accounts_json().expect("list_accounts_json");
        for account in &layout {
            let _ = profile
                .list_mailboxes_json(account.id)
                .expect("list_mailboxes_json");
            let _ = profile
                .mailbox_counts_json(account.id)
                .expect("mailbox_counts_json");
            let _ = profile
                .list_drafts_json(account.id)
                .expect("list_drafts_json");
        }
        let _ = profile
            .list_smart_mailboxes_json()
            .expect("list_smart_mailboxes_json");
        let _ = profile
            .list_snoozed_messages_json(500, 0)
            .expect("list_snoozed_messages_json");
        let _ = profile
            .list_messages_by_role_page_json("inbox", None, 500)
            .expect("list_messages_by_role_page_json");
    });
    // The deferred refresh, timed outside the row (same warm-up/iteration
    // counts as `measure`).
    for _ in 0..(WARMUP_ITERS + MEASURED_ITERS) {
        let contacts_started = Instant::now();
        let _ = profile
            .refresh_contacts_json()
            .expect("refresh_contacts_json");
        contacts_ms.push(contacts_started.elapsed().as_secs_f64() * 1000.0);
    }
    lines.push((
        perf_line(b("launch_data_path"), launch_p95),
        launch_p50,
        launch_p95,
    ));
    // `measure` ran WARMUP_ITERS untimed passes first; keep only the timed ones.
    let (contacts_p50, contacts_p95) = p50_p95(contacts_ms.split_off(WARMUP_ITERS));
    println!(
        "INFO deferred_contacts_refresh (off the launch path) p50={contacts_p50:.3}ms p95={contacts_p95:.3}ms"
    );

    // #3 list_first_page.
    let (first_p50, first_p95) = measure(|| {
        let _ = profile
            .list_messages_page_json(acct_a.id, &acct_a_inbox_id, None, 500)
            .expect("list_messages_page_json (first)");
    });
    lines.push((
        perf_line(b("list_first_page"), first_p95),
        first_p50,
        first_p95,
    ));

    // #4 list_deep_page -- walk to page 81 (~40k rows deep) untimed once,
    // then time that one page repeatedly from the same cursor.
    let mut cursor_json: Option<String> = None;
    for _ in 0..80 {
        let page_json = profile
            .list_messages_page_json(acct_a.id, &acct_a_inbox_id, cursor_json.as_deref(), 500)
            .expect("walk to deep page");
        let page: serde_json::Value = serde_json::from_str(&page_json).expect("parse page");
        cursor_json = (!page["next"].is_null())
            .then(|| serde_json::to_string(&page["next"]).expect("serialize cursor"));
        if cursor_json.is_none() {
            break;
        }
    }
    let (deep_p50, deep_p95) = measure(|| {
        let _ = profile
            .list_messages_page_json(acct_a.id, &acct_a_inbox_id, cursor_json.as_deref(), 500)
            .expect("list_messages_page_json (deep)");
    });
    lines.push((perf_line(b("list_deep_page"), deep_p95), deep_p50, deep_p95));

    // #5 list_depth_ratio (blocking shape check).
    let depth_ratio = if first_p95 > 0.0 {
        deep_p95 / first_p95
    } else {
        1.0
    };
    lines.push((
        perf_line(b("list_depth_ratio"), depth_ratio),
        depth_ratio,
        depth_ratio,
    ));

    // #6 unified_first_page.
    let (unified_p50, unified_p95) = measure(|| {
        let _ = profile
            .list_messages_by_role_page_json("inbox", None, 500)
            .expect("list_messages_by_role_page_json (unified first)");
    });
    lines.push((
        perf_line(b("unified_first_page"), unified_p95),
        unified_p50,
        unified_p95,
    ));

    // #7 counts_all_accounts.
    let (counts_p50, counts_p95) = measure(|| {
        for account in &layout {
            let _ = profile
                .mailbox_counts_json(account.id)
                .expect("mailbox_counts_json");
        }
    });
    lines.push((
        perf_line(b("counts_all_accounts"), counts_p95),
        counts_p50,
        counts_p95,
    ));

    // #8 smart_mailbox_recount.
    let (recount_p50, recount_p95) = measure(|| {
        let _ = profile
            .recompute_all_smart_mailbox_counts_json()
            .expect("recompute_all_smart_mailbox_counts_json");
    });
    lines.push((
        perf_line(b("smart_mailbox_recount"), recount_p95),
        recount_p50,
        recount_p95,
    ));

    // #9-12 search -- the shapes validated above, in budget order.
    for shape in &shapes {
        let (p50, p95) = measure(|| {
            let _ = profile
                .search_messages_across_accounts_json(&shape.query, SEARCH_LIMIT)
                .expect("search");
        });
        lines.push((perf_line(b(shape.budget), p95), p50, p95));
    }

    drop(profile);

    // #13 qresync_noop_resync (S30): one warm sync with QRESYNC enabled,
    // then the wall time of a whole resync against the SAME unchanged
    // provider (see `noop_resync_ms`).
    let qresync_dir = dir.join("qresync-scratch");
    std::fs::create_dir_all(&qresync_dir).expect("create qresync scratch dir");
    let qdb = Database::open(&qresync_dir.join("castellum.db")).expect("open qresync scratch db");
    let mut rng = Rng::new(2266);
    let mut provider = SyntheticProvider::new(acct_a, &mut rng, true);
    sync_with_provider(
        &qdb,
        acct_a.id,
        acct_a.email,
        None,
        &mut provider,
        "synthetic-host",
    )
    .expect("qresync warm sync");
    let (qresync_p50, qresync_p95) =
        measure_value(|| noop_resync_ms(&qdb, acct_a, Reconcile::Qresync));
    lines.push((
        perf_line(b("qresync_noop_resync"), qresync_p95),
        qresync_p50,
        qresync_p95,
    ));
    // The plan's comparison (Task 5, row 13): the same no-op resync through
    // the pre-S30 full UID+FLAGS scan. Informational, fewer iterations (each
    // pass reconciles every stored message).
    let full_scan_p50 = {
        noop_resync_ms(&qdb, acct_a, Reconcile::FullScan); // warm-up
        let samples: Vec<f64> = (0..5)
            .map(|_| noop_resync_ms(&qdb, acct_a, Reconcile::FullScan))
            .collect();
        p50_p95(samples).0
    };
    println!(
        "INFO noop_resync qresync_p50={qresync_p50:.3}ms full_scan_p50={full_scan_p50:.3}ms \
         full_scan/qresync={:.1}x",
        full_scan_p50 / qresync_p50
    );

    // #14 embed_queue_next_job (advisory) -- against the SEEDED profile's
    // 20k `embed_message` backlog. (It used to query the QRESYNC scratch
    // DB above, whose descriptor-only sync enqueues no embed jobs: an empty
    // queue, timed at 0.01 ms.)
    let seeded = Database::open(&dir.join("castellum.db")).expect("open seeded db");
    let now = 4_100_000_000i64; // Past the snooze/embed fixture's timestamps.
    match seeded.next_due_ai_job(now).expect("next_due_ai_job") {
        Some(job) if job.feature == "embed_message" => {}
        other => {
            eprintln!(
                "\nFAIL: embed_queue_next_job: expected the seeded embed_message backlog, \
                 found {:?} -- refusing to time an empty queue",
                other.map(|j| j.feature)
            );
            scratch.cleanup();
            std::process::exit(1);
        }
    }
    let (embed_p50, embed_p95) = measure(|| {
        let _ = seeded.next_due_ai_job(now).expect("next_due_ai_job");
    });
    // Queue order is a property of the fixture, not measured here:
    // seed_profile enqueues each mailbox's hydrated bodies newest message
    // first (hydrate_bodies walks uid ascending == sent_at descending), and
    // the queue drains `ORDER BY priority ASC, id ASC`. S64 does not change
    // queue order; that belongs to the embeddings pipeline.
    println!("INFO embed_queue order=newest-first-per-mailbox (seed_profile enqueue order, not measured)");
    lines.push((
        perf_line(b("embed_queue_next_job"), embed_p95),
        embed_p50,
        embed_p95,
    ));
    drop(seeded);
    drop(qdb);

    // #15 search_body_coverage_count (advisory) -- #3633 G13: the app runs
    // this COUNT next to every search to word "bodies of N messages are not
    // downloaded yet". Timed on the widest scope (every account); the
    // account and mailbox scopes are index-prefix narrowings of it.
    let profile = open_profile(&dir).expect("open_profile (coverage)");
    let coverage = profile
        .body_hydration_scope_json(None, None, None)
        .expect("body_hydration_scope_json");
    let scope: serde_json::Value = serde_json::from_str(&coverage).expect("coverage json");
    assert!(
        scope["total"].as_i64().unwrap_or(0) > 0,
        "refusing to time an empty body-coverage scope: {coverage}"
    );
    let (cov_p50, cov_p95) = measure(|| {
        let _ = profile
            .body_hydration_scope_json(None, None, None)
            .expect("body_hydration_scope_json");
    });
    let (cov_acct_p50, _) = measure(|| {
        let _ = profile
            .body_hydration_scope_json(Some(acct_a.id), Some(&acct_a_inbox_id), None)
            .expect("body_hydration_scope_json (mailbox)");
    });
    println!("INFO body_coverage scope=all {coverage} mailbox_scope_p50={cov_acct_p50:.3}ms");
    lines.push((
        perf_line(b("search_body_coverage_count"), cov_p95),
        cov_p50,
        cov_p95,
    ));

    println!();
    for (line, p50, _p95) in &lines {
        println!("{line}  (p50={p50:.3})");
    }

    let any_fail = budgets
        .iter()
        .zip(lines.iter())
        .any(|(bud, (_, _, p95))| matches!(perf_common::judge(bud, *p95), Verdict::Fail));
    scratch.cleanup();
    if any_fail && perf_common::advisory() {
        println!("\nWARN(adv): read-path ceilings exceeded, ignored (CASTELLUM_PERF_ADVISORY=1).");
    } else if any_fail {
        eprintln!("\nFAIL: one or more read-path perf budgets exceeded ceiling.");
        std::process::exit(1);
    }
    println!("\nPASS: all read-path perf budgets within ceiling.");
}

/// Which reconciliation a no-op resync should take.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Reconcile {
    /// S30: the provider advertises QRESYNC and answers with an empty delta.
    Qresync,
    /// Pre-S30: a full UID+FLAGS scan of every mailbox.
    FullScan,
}

/// Wall time of one whole no-op resync of `account` against `db` (already
/// synced once), in ms: `sync_with_provider` end to end -- mailbox list,
/// SELECT (QRESYNC or plain), the descriptor check, reconciliation
/// (`imap.apply_qresync_delta` or the UID+FLAGS scan), cursor bookkeeping
/// -- against the in-memory provider. Not a phase sum: round 3 found that
/// summing `db.*`/`core.*` phases read 0.000 ms, because none of them run
/// on a QRESYNC no-op. Asserts that nothing was fetched and that every
/// mailbox took the requested path, so the number is what the row claims.
fn noop_resync_ms(db: &Database, account: &AccountSpec, path: Reconcile) -> f64 {
    // Building the provider generates every descriptor again; not part of a
    // resync, so it stays outside the timed region.
    let mut provider =
        SyntheticProvider::new(account, &mut Rng::new(2266), path == Reconcile::Qresync);
    // Timing stays on only to verify the path below; its cost (a clock
    // read and a push per phase) is part of what is measured, and small.
    sync_timing::enable();
    let started = Instant::now();
    let report = sync_with_provider(
        db,
        account.id,
        account.email,
        None,
        &mut provider,
        "synthetic-host",
    )
    .expect("no-op resync");
    let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    let timings = sync_timing::take().expect("sync_timing enabled");
    assert_eq!(
        report.mailboxes.iter().map(|m| m.fetched).sum::<usize>(),
        0,
        "a no-op resync must fetch nothing"
    );
    let mailboxes = account.mailboxes.len();
    let calls = |phase: &str| timings.samples.get(phase).map_or(0, Vec::len);
    let (qresync_selects, full_scans) =
        (calls("imap.select_qresync"), calls("imap.list_uid_flags"));
    match path {
        Reconcile::Qresync => assert!(
            qresync_selects == mailboxes && full_scans == 0,
            "expected the QRESYNC path in all {mailboxes} mailboxes, got \
             {qresync_selects} QRESYNC selects and {full_scans} full scans"
        ),
        Reconcile::FullScan => assert!(
            full_scans == mailboxes && qresync_selects == 0,
            "expected a full scan in all {mailboxes} mailboxes, got \
             {full_scans} full scans and {qresync_selects} QRESYNC selects"
        ),
    }
    assert!(
        wall_ms > 0.0,
        "a no-op resync still selects every mailbox; 0 ms means the timer is broken"
    );
    wall_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn judge_boundaries_via_perf_line() {
        let b = Budget {
            name: "x",
            unit: "ms",
            target: 10.0,
            ceiling: 40.0,
        };
        assert!(perf_line(&b, 10.0).ends_with("PASS"));
        assert!(perf_line(&b, 25.0).ends_with("WARN"));
        assert!(perf_line(&b, 41.0).ends_with("FAIL"));
    }

    #[test]
    fn perf_line_format() {
        let b = Budget {
            name: "widget",
            unit: "ms",
            target: 10.0,
            ceiling: 40.0,
        };
        let line = perf_line(&b, 5.0);
        assert!(line.starts_with("PERF widget measured=5.000ms target=10 ceiling=40 "));
    }

    #[test]
    fn zipfish_draw_makes_the_highest_index_the_most_common_word() {
        // P(k) = (2k+1)/W^2: over 400k draws word1999 expects ~400 hits,
        // word0 ~0.1. Round 3 found the gate labelling word0 "most common".
        let mut rng = Rng::new(2266);
        let mut counts = vec![0u32; perf_common::WORD_POOL_SIZE as usize];
        for _ in 0..400_000 {
            counts[perf_common::zipfish_word(&mut rng) as usize] += 1;
        }
        let last = counts.len() - 1;
        assert!(counts[last] > 300, "word{last} drew {}", counts[last]);
        assert!(counts[0] <= 2, "word0 drew {}", counts[0]);
        assert_eq!(
            perf_common::most_common_word(),
            perf_common::pool_word(last as u64)
        );
    }

    #[test]
    fn search_shape_rejects_hit_counts_outside_its_band() {
        let common = SearchShape {
            budget: "search_common_term",
            query: "w".into(),
            hits: 100..=usize::MAX,
        };
        assert!(
            common.check(0).is_err(),
            "a zero-hit common term is vacuous"
        );
        assert!(common.check(99).is_err());
        assert!(common.check(461).is_ok());
        let rare = SearchShape {
            budget: "search_rare_term",
            query: "w".into(),
            hits: RARE_HITS,
        };
        assert!(rare.check(0).is_err(), "a zero-hit rare term is vacuous");
        assert!(rare.check(1).is_ok());
        assert!(rare.check(20).is_ok());
        assert!(rare.check(21).is_err());
    }

    #[test]
    fn pick_rare_word_skips_absent_words() {
        // word0..word2 absent from the corpus, word3 in 2 messages.
        let hits = |w: &str| {
            let k: usize = w.trim_start_matches("word").parse().unwrap();
            k.saturating_sub(2) * 2
        };
        assert_eq!(pick_rare_word(hits), Some(("word3".to_string(), 2)));
        assert_eq!(pick_rare_word(|_| 0), None);
        assert_eq!(pick_rare_word(|_| 21), None);
    }

    /// Round 3: `qresync_noop_resync` summed only `db.*`/`core.*` phases,
    /// none of which run on a QRESYNC no-op, so it always read 0.000 ms and
    /// could never fail. The resync still selects every mailbox and reads
    /// and writes the sync cursor -- real work that must measure > 0.
    #[test]
    fn noop_resync_measures_the_work_it_does() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "castellum-read-perf-gate-test-{}-{stamp}",
            std::process::id()
        ));
        let _cleanup = ScratchDirs(vec![dir.clone()]);
        std::fs::create_dir_all(&dir).expect("create test dir");
        let db = Database::open(&dir.join("castellum.db")).expect("open test db");
        let layout = corpus_layout(600);
        let account = &layout[0];
        let mut provider = SyntheticProvider::new(account, &mut Rng::new(2266), true);
        sync_with_provider(
            &db,
            account.id,
            account.email,
            None,
            &mut provider,
            "synthetic-host",
        )
        .expect("warm sync");

        let qresync = noop_resync_ms(&db, account, Reconcile::Qresync);
        assert!(qresync > 0.0, "QRESYNC no-op resync measured {qresync} ms");
        let full_scan = noop_resync_ms(&db, account, Reconcile::FullScan);
        assert!(
            full_scan > 0.0,
            "full-scan no-op resync measured {full_scan} ms"
        );
    }

    #[test]
    fn depth_ratio_check_fails_when_deep_is_3x_first() {
        let b = Budget {
            name: "list_depth_ratio",
            unit: "x",
            target: 2.0,
            ceiling: 2.0,
        };
        let first = 10.0;
        let deep = 30.0;
        assert!(matches!(
            perf_common::judge(&b, deep / first),
            Verdict::Fail
        ));
    }
}
