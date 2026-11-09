// Castellum - deterministic 100k-message seeded profile generator (S64).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Persists a realistic profile (two accounts, five mailboxes, threads, 20%
// bodies with an embed-job backlog, three smart mailboxes, 50 snoozed
// messages) by running the REAL sync path (`sync_with_provider`) against the
// in-memory `perf_common::SyntheticProvider`, so `read_perf_gate.rs` (Task 5)
// and the app's own perf XCTests (Task 6) can budget against a fixture that
// looks like the app's own on-disk layout, not a hand-rolled schema.
//
// Usage:
//   cargo run --release -p castellum-core --example seed_profile -- \
//       <out-dir> [--messages 100000] [--seed 2266] [--body-share 0.2] \
//       [--descriptor-only]
//
// Two profile modes (recorded in the manifest as `mode`):
//   bodies          (default, M2) the hydrated 20% carry a synthetic body that
//                   is INDEXED into `message_fts` exactly as real hydration
//                   does (`attachments: Some(..)` is what makes
//                   `upsert_message` re-index the body), so every search
//                   budget is measured against real FTS postings.
//   descriptor-only (`--descriptor-only`) the pre-M2 profile: bodies are stored
//                   but never reach FTS (`attachments: None` reads as a
//                   descriptor retry). Kept so the sync-gate numbers and the
//                   old search baselines stay comparable.
//
// Body size distribution (bytes, uniform within each tier, drawn from the
// seeded RNG so two runs are byte-identical):
//   75.0%   0.3 -   4 KiB   (the everyday mail: a few KB)
//   22.0%   4   -  24 KiB   (newsletters, threads with quoted replies)
//    2.7%  24   -  96 KiB   (long threads, reports)
//    0.3%  96   - 384 KiB   (the heavy tail: pasted logs, exports)
// Words come from the shared 2,000-word Zipf-ish pool (`word1999` the most
// common, `word0` the rarest), the same pool the subjects use, so a common and
// a rare term both occur in bodies.
//
// Idempotent: re-running with an unchanged `<out-dir>/perf-profile.json`
// (same seed/size) exits 0 immediately without touching the profile again --
// cheap local re-use and a fast CI seed step once the fixture is warm.
//
// No message content anywhere in stdout: only counts, byte totals and
// timings are ever printed (Global Constraints).

#[path = "perf_common/mod.rs"]
mod perf_common;

use castellum_core::sync::sync_with_provider;
use castellum_db::{Database, SmartMailboxUpsert, AI_PRIORITY_BACKGROUND};
use castellum_models::{Address, AttachmentDescriptor, NewMessage};
use castellum_providers::{RemoteAddress, RemoteMessageDescriptor};
use perf_common::{corpus_layout, AccountSpec, Rng, SyntheticProvider};
use std::path::{Path, PathBuf};
use std::time::Instant;

const DEFAULT_MESSAGES: i64 = 100_000;
const DEFAULT_SEED: u64 = 2266;
const DEFAULT_BODY_SHARE: f64 = 0.2;
const WORDS_PER_BODY_MIN: u64 = 40;
const WORDS_PER_BODY_MAX: u64 = 400;
const SNOOZE_COUNT: usize = 50;
/// Far enough in the future that a snooze fixture never comes due while a
/// perf run is measuring it.
const SNOOZE_UNTIL_EPOCH: i64 = 4_000_000_000;

struct Args {
    out_dir: PathBuf,
    messages: i64,
    seed: u64,
    body_share: f64,
    /// `--descriptor-only`: the pre-M2 profile (bodies stored, not indexed).
    descriptor_only: bool,
}

/// Manifest `mode` strings.
const MODE_BODIES: &str = "bodies";
const MODE_DESCRIPTOR_ONLY: &str = "descriptor-only";

fn parse_args() -> Args {
    let mut positional: Vec<String> = Vec::new();
    let mut messages = DEFAULT_MESSAGES;
    let mut seed = DEFAULT_SEED;
    let mut body_share = DEFAULT_BODY_SHARE;
    let mut descriptor_only = false;

    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--messages" => {
                messages = iter
                    .next()
                    .expect("--messages needs a value")
                    .parse()
                    .expect("--messages must be an integer");
            }
            "--seed" => {
                seed = iter
                    .next()
                    .expect("--seed needs a value")
                    .parse()
                    .expect("--seed must be an integer");
            }
            "--body-share" => {
                body_share = iter
                    .next()
                    .expect("--body-share needs a value")
                    .parse()
                    .expect("--body-share must be a float");
            }
            "--descriptor-only" => descriptor_only = true,
            other => positional.push(other.to_string()),
        }
    }

    let out_dir = positional.into_iter().next().expect(
        "usage: seed_profile <out-dir> [--messages N] [--seed N] [--body-share F] \
             [--descriptor-only]",
    );

    Args {
        out_dir: PathBuf::from(out_dir),
        messages,
        seed,
        body_share,
        descriptor_only,
    }
}

#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
struct Manifest {
    seed: u64,
    messages: i64,
    accounts: std::collections::BTreeMap<String, std::collections::BTreeMap<String, i64>>,
    bodies: i64,
    smart_mailboxes: i64,
    snoozed: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    db_bytes: Option<u64>,
    /// `bodies` or `descriptor-only`; a manifest from before M2 has no field
    /// and is, by construction, descriptor-only.
    #[serde(default = "default_mode")]
    mode: String,
    /// Sum of the synthetic body lengths written (bodies mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    body_bytes: Option<u64>,
    /// Footprint, measured by the seeder while the connection is still open:
    /// `castellum.db` alone, the WAL after the last write, the WAL after an
    /// explicit `wal_checkpoint(TRUNCATE)`. Read by `perf_footprint`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    main_db_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wal_bytes_after_seed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wal_bytes_after_checkpoint: Option<u64>,
}

fn default_mode() -> String {
    MODE_DESCRIPTOR_ONLY.to_string()
}

fn manifest_path(out_dir: &Path) -> PathBuf {
    out_dir.join("perf-profile.json")
}

/// `<out-dir>` already holds a manifest for the same seed/size: reuse it
/// without touching the profile (Task 2's cheap-local-reuse contract).
fn existing_manifest_matches(out_dir: &Path, seed: u64, messages: i64, mode: &str) -> bool {
    let Ok(bytes) = std::fs::read(manifest_path(out_dir)) else {
        return false;
    };
    let Ok(m) = serde_json::from_slice::<Manifest>(&bytes) else {
        return false;
    };
    m.seed == seed
        && m.messages == messages
        && m.mode == mode
        && out_dir.join("castellum.db").exists()
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

fn mailbox_id_for(account_id: &str, path: &str) -> String {
    format!("{account_id}:{path}")
}

/// A deterministic 40-400 word plain-text body from the shared word pool,
/// via the same Zipf-ish draw `synthetic_descriptors` uses for subjects.
fn generate_body(rng: &mut Rng) -> String {
    let n_words = WORDS_PER_BODY_MIN + rng.below(WORDS_PER_BODY_MAX - WORDS_PER_BODY_MIN + 1);
    (0..n_words)
        .map(|_| perf_common::pool_word(perf_common::zipfish_word(rng)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Target body length in bytes from the tiered distribution in the header.
fn body_target_bytes(rng: &mut Rng) -> usize {
    let (lo, span) = match rng.below(1000) {
        0..=749 => (300, 3_800),
        750..=969 => (4_096, 20_480),
        970..=996 => (24_576, 73_728),
        _ => (98_304, 294_912),
    };
    (lo + rng.below(span)) as usize
}

/// A deterministic body of at least `target` bytes: pool words (Zipf-ish, so
/// common and rare terms both occur), a newline every 12 words like wrapped
/// prose.
fn generate_sized_body(rng: &mut Rng, target: usize) -> String {
    let mut body = String::with_capacity(target + 16);
    let mut n = 0u32;
    while body.len() < target {
        if n > 0 {
            body.push(if n.is_multiple_of(12) { '\n' } else { ' ' });
        }
        body.push_str(&perf_common::pool_word(perf_common::zipfish_word(rng)));
        n += 1;
    }
    body
}

/// Body-indexed hydration (M2): every `stride`-th descriptor of the mailbox
/// is re-upserted with its FULL envelope (so addresses, references, flags and
/// attachment state stay exactly what the sync wrote) plus a synthetic body,
/// and `attachments: Some(..)` -- the marker real hydration sets -- so
/// `upsert_message` re-indexes the body into `message_fts`. Returns the ids
/// hydrated and the total body bytes.
fn hydrate_bodies_indexed(
    db: &Database,
    rng: &mut Rng,
    account_id: &str,
    mailbox_path: &str,
    descriptors: &[RemoteMessageDescriptor],
    body_share: f64,
) -> (Vec<String>, u64) {
    let mailbox_id = mailbox_id_for(account_id, mailbox_path);
    let stride = (1.0 / body_share).round().max(1.0) as usize;
    let mut ids = Vec::new();
    let mut total_bytes = 0u64;
    for d in descriptors.iter().step_by(stride) {
        let target = body_target_bytes(rng);
        let body = generate_sized_body(rng, target);
        total_bytes += body.len() as u64;
        let snippet: String = body.chars().take(160).collect();
        let mut addresses = Vec::new();
        let mut push = |kind: &str, a: &RemoteAddress| {
            addresses.push(Address {
                kind: kind.to_string(),
                name: a.name.clone(),
                address: a.address.clone(),
            });
        };
        if let Some(from) = &d.from {
            push("from", from);
        }
        d.to.iter().for_each(|a| push("to", a));
        d.cc.iter().for_each(|a| push("cc", a));
        let has_attachments = d.has_attachments.unwrap_or(false);
        let attachments = if has_attachments {
            vec![AttachmentDescriptor {
                ordinal: 0,
                filename: Some(format!("attachment-{}.pdf", d.uid)),
                content_type: Some("application/pdf".to_string()),
                size_bytes: Some(52_000),
                section: "2".to_string(),
                content_id: None,
                is_inline: false,
                transfer_encoding: Some("base64".to_string()),
            }]
        } else {
            vec![]
        };
        let id = format!("{mailbox_id}:{}", d.uid);
        let new_message = NewMessage {
            id: id.clone(),
            account_id: account_id.to_string(),
            mailbox_id: mailbox_id.clone(),
            provider_uid: Some(d.uid),
            rfc822_message_id: d.rfc822_message_id.clone(),
            subject: d.subject.clone(),
            from_name: d.from.as_ref().and_then(|a| a.name.clone()),
            from_address: d.from.as_ref().map(|a| a.address.clone()),
            sent_at: d.sent_at,
            snippet: Some(snippet),
            size_bytes: Some(body.len() as i64),
            has_attachments,
            attachments: Some(attachments),
            is_read: d.is_seen,
            is_starred: d.is_flagged,
            addresses,
            text_body: Some(body),
            html_body: None,
            sanitized_html: None,
            has_remote_content: false,
            references: d.references.clone(),
        };
        db.upsert_message(&new_message)
            .expect("upsert_message (indexed body hydration)");
        ids.push(id);
    }
    (ids, total_bytes)
}

/// Hydrate a deterministic slice of a mailbox's messages with a synthetic
/// body, exactly as the real hydration path's `NewMessage` fields would
/// (see `sync.rs::new_message_from_raw`'s callers, "`bodies_fetched`"):
/// leaves envelope fields as `None` so `Database::upsert_message`'s
/// `ON CONFLICT` COALESCE keeps the descriptor-imported values, and only
/// the body/snippet/size columns change. Returns the message ids hydrated,
/// for the embed-job backlog this seeds.
fn hydrate_bodies(
    db: &Database,
    rng: &mut Rng,
    account_id: &str,
    mailbox_path: &str,
    count: i64,
    body_share: f64,
) -> Vec<String> {
    let mailbox_id = mailbox_id_for(account_id, mailbox_path);
    // Deterministic ~body_share of uids: every Nth uid, N = round(1/share).
    let stride = (1.0 / body_share).round().max(1.0) as i64;
    let mut hydrated_ids = Vec::new();
    let mut uid = 1;
    while uid <= count {
        let body = generate_body(rng);
        let snippet: String = body.chars().take(160).collect();
        let id = format!("{mailbox_id}:{uid}");
        let new_message = NewMessage {
            id: id.clone(),
            account_id: account_id.to_string(),
            mailbox_id: mailbox_id.clone(),
            provider_uid: Some(uid),
            rfc822_message_id: None,
            subject: None,
            from_name: None,
            from_address: None,
            sent_at: None,
            snippet: Some(snippet),
            size_bytes: Some(body.len() as i64),
            has_attachments: false,
            attachments: None,
            is_read: false,
            is_starred: false,
            addresses: vec![],
            text_body: Some(body),
            html_body: None,
            sanitized_html: None,
            has_remote_content: false,
            references: vec![],
        };
        db.upsert_message(&new_message)
            .expect("upsert_message (body hydration)");
        hydrated_ids.push(id);
        uid += stride;
    }
    hydrated_ids
}

/// Per-account seed result: mailbox counts, hydrated message ids, body bytes.
type Seeded = (std::collections::BTreeMap<String, i64>, Vec<String>, u64);

fn seed_account(
    db: &Database,
    account: &AccountSpec,
    rng: &mut Rng,
    body_share: f64,
    descriptor_only: bool,
) -> Seeded {
    let mut provider = SyntheticProvider::new(account, rng, false);
    sync_with_provider(
        db,
        account.id,
        account.email,
        None,
        &mut provider,
        "synthetic-host",
    )
    .expect("sync_with_provider (seed account)");

    let mut counts = std::collections::BTreeMap::new();
    let mut all_hydrated = Vec::new();
    let mut body_bytes = 0u64;
    for mailbox in &account.mailboxes {
        counts.insert(mailbox.path.to_string(), mailbox.count);
        if descriptor_only {
            let hydrated =
                hydrate_bodies(db, rng, account.id, mailbox.path, mailbox.count, body_share);
            all_hydrated.extend(hydrated);
        } else {
            let (hydrated, bytes) = hydrate_bodies_indexed(
                db,
                rng,
                account.id,
                mailbox.path,
                provider.descriptors_of(mailbox.path),
                body_share,
            );
            all_hydrated.extend(hydrated);
            body_bytes += bytes;
        }
    }
    (counts, all_hydrated, body_bytes)
}

fn seed_smart_mailboxes(db: &Database, domain0: &str) {
    db.upsert_smart_mailbox(SmartMailboxUpsert {
        id: Some("smart-unread"),
        name: "Unread",
        query: "is:unread",
        symbol: None,
        group: None,
        scope: None,
    })
    .expect("create smart mailbox: Unread");
    db.upsert_smart_mailbox(SmartMailboxUpsert {
        id: Some("smart-ana"),
        name: "Ana",
        query: &format!("from:ana@{domain0}"),
        symbol: None,
        group: None,
        scope: None,
    })
    .expect("create smart mailbox: Ana");
    db.upsert_smart_mailbox(SmartMailboxUpsert {
        id: Some("smart-big-attachments"),
        name: "Big attachments",
        query: "has:attachment before:2030-01-01",
        symbol: None,
        group: None,
        scope: None,
    })
    .expect("create smart mailbox: Big attachments");
    db.recompute_all_smart_mailbox_counts()
        .expect("recompute_all_smart_mailbox_counts");
}

fn seed_snoozed(db: &Database, account_id: &str, mailbox_path: &str, count: usize) {
    let mailbox_id = mailbox_id_for(account_id, mailbox_path);
    for uid in 1..=count as i64 {
        let id = format!("{mailbox_id}:{uid}");
        db.snooze_message(&id, SNOOZE_UNTIL_EPOCH)
            .unwrap_or_else(|e| panic!("snooze_message({id}): {e}"));
    }
}

fn main() {
    let args = parse_args();
    let mode = if args.descriptor_only {
        MODE_DESCRIPTOR_ONLY
    } else {
        MODE_BODIES
    };
    println!(
        "seed_profile: {} messages, seed {}, body-share {}, mode {mode}, out-dir {}",
        args.messages,
        args.seed,
        args.body_share,
        args.out_dir.display()
    );

    if existing_manifest_matches(&args.out_dir, args.seed, args.messages, mode) {
        println!("seed_profile: existing profile matches seed/size -- reusing, no write.");
        std::process::exit(0);
    }

    // A non-empty, non-matching out-dir would silently mix an old profile
    // with a new seed/size -- refuse rather than corrupt a fixture.
    if args.out_dir.exists()
        && std::fs::read_dir(&args.out_dir)
            .map(|mut rd| rd.next().is_some())
            .unwrap_or(false)
    {
        eprintln!(
            "seed_profile: {} is non-empty and does not match this seed/size; \
             remove it first (fixtures are never partially reused).",
            args.out_dir.display()
        );
        std::process::exit(1);
    }

    std::fs::create_dir_all(&args.out_dir).expect("create out-dir");
    let db_path = args.out_dir.join("castellum.db");
    let db = Database::open(&db_path).expect("open profile db");

    let started = Instant::now();
    let layout = corpus_layout(args.messages);
    let mut rng = Rng::new(args.seed);

    let mut accounts_manifest = std::collections::BTreeMap::new();
    let mut hydrated_total: Vec<String> = Vec::new();
    let mut body_bytes_total = 0u64;
    let mut domain0 = String::new();
    for account in &layout {
        let (counts, hydrated, body_bytes) = seed_account(
            &db,
            account,
            &mut rng,
            args.body_share,
            args.descriptor_only,
        );
        accounts_manifest.insert(account.id.to_string(), counts);
        hydrated_total.extend(hydrated);
        body_bytes_total += body_bytes;
        if domain0.is_empty() {
            // Matches `synthetic_sender`'s `domain0.test` (sender index 0).
            domain0 = "domain0.test".to_string();
        }
    }

    // The embeddings backlog Task 5 measures: one background `embed_message`
    // job per hydrated body, exactly as `sync.rs::enqueue_embed_job` does.
    for id in &hydrated_total {
        let payload = serde_json::json!({ "message_id": id }).to_string();
        db.enqueue_ai_job(
            Some(layout[0].id),
            "embed_message",
            &payload,
            AI_PRIORITY_BACKGROUND,
        )
        .expect("enqueue_ai_job (embed_message)");
    }

    seed_smart_mailboxes(&db, &domain0);

    let inbox_path = layout[0].mailboxes[0].path;
    seed_snoozed(&db, layout[0].id, inbox_path, SNOOZE_COUNT);

    db.refresh_contact_aggregates(0)
        .expect("refresh_contact_aggregates");

    // Footprint (M2): the WAL as the last write left it, then after an
    // explicit TRUNCATE checkpoint from a second connection (the app's own
    // post-migration checkpoint does the same). `castellum.db` alone is
    // measured after the checkpoint.
    let wal_path = args.out_dir.join("castellum.db-wal");
    let wal_len = || std::fs::metadata(&wal_path).map_or(0, |m| m.len());
    let wal_bytes_after_seed = wal_len();
    {
        let conn = rusqlite::Connection::open(&db_path).expect("open for checkpoint");
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .expect("wal_checkpoint(TRUNCATE)");
    }
    let wal_bytes_after_checkpoint = wal_len();
    // Deterministic file size. The thread ids and every message-to-thread
    // assignment are identical across runs, but the `threads` rows are
    // inserted in a run-dependent order (so rowid order differs), which
    // changes how SQLite packs the table and its indexes: tens of pages of
    // spread for identical content. Rewrite `threads` in id order (foreign
    // keys are ON on this fresh connection: the bundled SQLite compiles with
    // SQLITE_DEFAULT_FOREIGN_KEYS=1. The only child table,
    // `thread_extractions`, is never seeded here, so the DELETE cascades to
    // nothing), then VACUUM to rebuild the file, then re-checkpoint to fold the VACUUM's
    // WAL frames back into `castellum.db`.
    {
        let conn = rusqlite::Connection::open(&db_path).expect("open for vacuum");
        conn.execute_batch(
            "BEGIN;
             CREATE TEMP TABLE threads_sorted AS SELECT * FROM threads ORDER BY id;
             DELETE FROM threads;
             INSERT INTO threads SELECT * FROM threads_sorted;
             DROP TABLE threads_sorted;
             COMMIT;
             VACUUM;
             PRAGMA wal_checkpoint(TRUNCATE);",
        )
        .expect("normalise threads order + VACUUM + checkpoint");
    }
    let main_db_bytes = std::fs::metadata(&db_path).map_or(0, |m| m.len());

    // Closing the connection also leaves nothing in the WAL (the measurement
    // fix #150 documented), so `db_bytes` is the checkpointed on-disk size.
    drop(db);
    let seconds = started.elapsed().as_secs_f64();
    let db_bytes = dir_size(&args.out_dir);

    let manifest = Manifest {
        seed: args.seed,
        messages: args.messages,
        accounts: accounts_manifest,
        bodies: hydrated_total.len() as i64,
        smart_mailboxes: 3,
        snoozed: SNOOZE_COUNT as i64,
        seconds: Some(seconds),
        db_bytes: Some(db_bytes),
        mode: mode.to_string(),
        body_bytes: Some(body_bytes_total),
        main_db_bytes: Some(main_db_bytes),
        wal_bytes_after_seed: Some(wal_bytes_after_seed),
        wal_bytes_after_checkpoint: Some(wal_bytes_after_checkpoint),
    };
    let manifest_json = serde_json::to_string(&manifest).expect("serialize manifest");
    std::fs::write(manifest_path(&args.out_dir), &manifest_json).expect("write manifest");

    println!("seed_profile: done in {seconds:.1}s -- {manifest_json}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_into(
        dir: &Path,
        messages: i64,
        seed: u64,
        descriptor_only: bool,
    ) -> (Manifest, Vec<(String, Option<String>)>) {
        std::fs::create_dir_all(dir).unwrap();
        let db = Database::open(&dir.join("castellum.db")).expect("open");
        let layout = corpus_layout(messages);
        let mut rng = Rng::new(seed);
        let mut accounts_manifest = std::collections::BTreeMap::new();
        let mut hydrated_total = Vec::new();
        for account in &layout {
            let (counts, hydrated, _) =
                seed_account(&db, account, &mut rng, DEFAULT_BODY_SHARE, descriptor_only);
            accounts_manifest.insert(account.id.to_string(), counts);
            hydrated_total.extend(hydrated);
        }
        let rows = list_all_id_subject_from(&db);
        drop(db);
        let manifest = Manifest {
            seed,
            messages,
            accounts: accounts_manifest,
            bodies: hydrated_total.len() as i64,
            smart_mailboxes: 0,
            snoozed: 0,
            seconds: None,
            db_bytes: None,
            mode: default_mode(),
            body_bytes: None,
            main_db_bytes: None,
            wal_bytes_after_seed: None,
            wal_bytes_after_checkpoint: None,
        };
        (manifest, rows)
    }

    /// Reads back `(id, subject)` for every message, ordered by id -- the
    /// determinism test's ground truth.
    fn list_all_id_subject_from(db: &Database) -> Vec<(String, Option<String>)> {
        // No public "list everything" accessor exists, so this test drives
        // the same account/mailbox listing surface the app uses: every
        // mailbox's role-page/limit-offset headers, concatenated.
        let mut out = Vec::new();
        for account in corpus_layout(2_000) {
            for mailbox in &account.mailboxes {
                let mailbox_id = mailbox_id_for(account.id, mailbox.path);
                let headers = db
                    .list_message_headers(account.id, &mailbox_id, mailbox.count + 10, 0)
                    .expect("list_message_headers");
                out.extend(headers.into_iter().map(|h| (h.id, h.subject)));
            }
        }
        out.sort();
        out
    }

    #[test]
    fn seeding_is_deterministic() {
        let dir_a = tempfile::tempdir().expect("tempdir a");
        let dir_b = tempfile::tempdir().expect("tempdir b");
        let (manifest_a, rows_a) = seed_into(dir_a.path(), 2_000, 2266, false);
        let (manifest_b, rows_b) = seed_into(dir_b.path(), 2_000, 2266, false);

        assert_eq!(manifest_a.accounts, manifest_b.accounts);
        assert_eq!(manifest_a.bodies, manifest_b.bodies);
        assert_eq!(rows_a, rows_b);
        assert!(!rows_a.is_empty());

        // Manifest per-mailbox counts match a direct COUNT(*) in the DB.
        let db = Database::open(&dir_a.path().join("castellum.db")).expect("reopen");
        for account in corpus_layout(2_000) {
            for mailbox in &account.mailboxes {
                let mailbox_id = mailbox_id_for(account.id, mailbox.path);
                let headers = db
                    .list_message_headers(account.id, &mailbox_id, mailbox.count + 10, 0)
                    .expect("list_message_headers");
                assert_eq!(
                    headers.len() as i64,
                    manifest_a.accounts[account.id][mailbox.path]
                );
            }
        }
    }

    /// Hit counts for the most common pool word and for a descriptor-time
    /// subject word, straight from `message_fts`, plus the number of rows
    /// whose `body` column is non-empty.
    fn fts_counts(dir: &Path) -> (i64, i64) {
        let conn = rusqlite::Connection::open(dir.join("castellum.db")).expect("open");
        let common: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM message_fts WHERE message_fts MATCH ?1",
                [perf_common::most_common_word()],
                |r| r.get(0),
            )
            .expect("fts count");
        let bodies: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM message_fts WHERE body <> ''",
                [],
                |r| r.get(0),
            )
            .expect("body count");
        (common, bodies)
    }

    #[test]
    fn body_indexed_mode_indexes_bodies_and_is_deterministic() {
        let dir_a = tempfile::tempdir().expect("tempdir a");
        let dir_b = tempfile::tempdir().expect("tempdir b");
        let (manifest, _) = seed_into(dir_a.path(), 2_000, 2266, false);
        seed_into(dir_b.path(), 2_000, 2266, false);

        let (common_a, bodies_a) = fts_counts(dir_a.path());
        let (common_b, bodies_b) = fts_counts(dir_b.path());
        assert_eq!((common_a, bodies_a), (common_b, bodies_b));
        // Every hydrated message has an indexed, non-empty body.
        assert_eq!(bodies_a, manifest.bodies);
        assert!(bodies_a > 0);
    }

    #[test]
    fn descriptor_only_mode_leaves_fts_bodies_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (manifest, _) = seed_into(dir.path(), 2_000, 2266, true);
        let (_, bodies) = fts_counts(dir.path());
        assert!(manifest.bodies > 0);
        assert_eq!(bodies, 0, "descriptor-only keeps the pre-M2 FTS shape");
    }

    #[test]
    fn body_sizes_follow_the_tiered_distribution() {
        let mut rng = Rng::new(7);
        let sizes: Vec<usize> = (0..4_000).map(|_| body_target_bytes(&mut rng)).collect();
        let under_4k = sizes.iter().filter(|&&n| n < 4_096).count();
        let over_96k = sizes.iter().filter(|&&n| n >= 98_304).count();
        // 75% / 0.3% with generous slack for 4,000 draws.
        assert!((2_800..=3_200).contains(&under_4k), "{under_4k}");
        assert!((1..=40).contains(&over_96k), "{over_96k}");
        let mean = sizes.iter().sum::<usize>() / sizes.len();
        assert!((4_000..=12_000).contains(&mean), "mean {mean}");
    }
}
