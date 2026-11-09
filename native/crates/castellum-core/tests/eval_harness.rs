// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - AI eval harness (S51).
//
// Runs the fixed, synthetic-only CA/ES/EN corpus under
// `tests/fixtures/eval/` through the REAL `ai::run`/`ai::defense` choke
// point end to end (via the public Profile API — the same path the app
// uses), scores each response against its fixture's rubric, and fails the
// build only when the aggregate score across the whole corpus drops below
// `REGRESSION_THRESHOLD`. This is the roadmap's S51 regression gate
// (project-docs/plans/2026-09-18-castellum-1.0-roadmap.md).
//
// Requires a locally reachable Ollama instance — same env vars and
// connectivity probe as `tests/ollama_e2e.rs`:
//
//   CASTELLUM_OLLAMA_URL   — base URL, e.g. "http://127.0.0.1:11434"
//   CASTELLUM_OLLAMA_MODEL — model name, e.g. "llama3.1:8b"
//
// S56-impl adds a small, parallel `probe_mlx` (see below) mirroring this
// shape for the MLX backend — the smaller of the two diffs the plan named
// (a parallel CASTELLUM_MLX_* probe vs. a backend-agnostic abstraction).
// It does NOT run the scored corpus through MLX: `MlxClient::generate()` is
// intentionally not implemented in S56-impl (Task 4 is stretch/deferred), so
// there is no real generation output for MLX to score yet. Once a future
// stage lands Task 4, `eval_corpus_scores_above_regression_threshold` should
// gain an MLX-selected sibling exercising the same corpus.
//
// The test is `#[ignore]` so it never runs in a plain `cargo test`; run it
// manually:
//
//   cargo test -p castellum-core --test eval_harness -- --ignored --nocapture
//
// If the env vars are absent OR the endpoint is unreachable, the test prints
// a skip notice and returns cleanly (exit 0) — it must NEVER fail the build
// for an environment with no Ollama wired up. This is the harness's core
// contract; `harness_skips_cleanly_without_env_vars` (below) pins it without
// needing a live model.
//
// Privacy: only a fixture's id, its response's character count, and which
// rubric checks passed/failed are ever logged. The model's actual response
// text is read in-process to score it and then dropped — never printed,
// never written to a file, never included in a CI artifact. The corpus
// itself is 100% invented (no real mail, ever — see CLAUDE.md).
//
// S54 extraction (M5): `extraction/<name>/fixture.json` fixtures carry a
// JSON-field rubric (expected action items, ISO dates, amounts) scored by
// `score_extraction` against the schema-validated `extract_thread_items`
// output. The scorer is unit-tested below on canned JSON with no model.
//
// Score history (M5): when `CASTELLUM_EVAL_JSONL` names a file, one JSON line
// per fixture is appended: fixture, feature, language, score, latency_ms,
// model, sha, ts. Never the model output or the fixture text (pinned by
// `jsonl_line_carries_only_the_allowed_keys`). At the end the harness prints
// the mean score per feature and language, and exactly one status line:
// `EVAL SKIPPED: no Ollama endpoint` or `EVAL RAN: <n> fixtures, mean <x>`,
// so CI can tell a green no-op from a real pass.

use castellum_core::ai::extraction::{parse_and_validate, StructuredValue};
use castellum_core::{close_profile, open_profile};
use castellum_db::Database;
use castellum_models::{Account, Mailbox, NewMessage};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Aggregate score (mean of every fixture's 0.0-1.0 rubric score) below which
/// the build fails. Checked in deliberately low-ish: the corpus is scored by
/// fuzzy heuristics against a non-deterministic model, not exact matching —
/// see `score_text` below. Raise this once real runs establish a baseline;
/// the point of S51 is to catch a REGRESSION, not to demand perfection.
const REGRESSION_THRESHOLD: f64 = 0.75;

// ---------------------------------------------------------------------------
// Ollama connectivity (identical pattern to tests/ollama_e2e.rs)
// ---------------------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tmp() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // Unique per RUN: pids recycle and these are never deleted, so a
    // pid+counter name reopens an earlier run's directory. See the note
    // in castellum_db::ai::tests::fresh_db.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    castellum_testutil::scratch_root().join(format!(
        "castellum-eval-harness-{}-{stamp}-{n}",
        std::process::id()
    ))
}

fn ollama_url() -> Option<String> {
    std::env::var("CASTELLUM_OLLAMA_URL").ok()
}

/// Why the harness would skip, decided purely from the value it was handed.
///
/// Split out from `ollama_url()` on purpose: the test that pins this contract
/// used to call `std::env::remove_var`, and a test binary is multi-threaded,
/// so that write races every other test reading the same variable. Under
/// `cargo test -- --include-ignored` it could clear the URL out from under
/// the real eval run and turn it into a silent skip — a green lane proving
/// nothing, which is the exact failure this harness exists to catch. A pure
/// function needs no such write.
fn skip_reason(url: Option<&str>) -> Option<&'static str> {
    match url {
        None => Some("CASTELLUM_OLLAMA_URL not set"),
        Some(u) if u.trim().is_empty() => Some("CASTELLUM_OLLAMA_URL is empty"),
        Some(_) => None,
    }
}

fn ollama_model() -> String {
    std::env::var("CASTELLUM_OLLAMA_MODEL").unwrap_or_else(|_| "llama3.1:8b".into())
}

/// Minimal Ollama connectivity probe: list models and verify the target model
/// is present. Returns the failure reason when the endpoint is unusable.
fn probe_ollama(url: &str, model: &str) -> Result<(), String> {
    use castellum_provider_ollama::{AiConfig, OllamaClient};
    use castellum_providers::AiBackend;

    let config = AiConfig {
        base_url: url.to_string(),
        request_timeout_secs: 10,
        connect_timeout_secs: 5,
    };

    let client = OllamaClient::connect(&config).map_err(|e| format!("connect: {e}"))?;
    let models = client
        .list_models()
        .map_err(|e| format!("list models: {e}"))?;
    let found = models.iter().any(|m| m.name.starts_with(model));
    if !found {
        return Err(format!(
            "model {model:?} not found; available: {:?}",
            models.iter().map(|m| &m.name).collect::<Vec<_>>()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// MLX connectivity (S56-impl) — parallel to the Ollama probe above, gated by
// its own CASTELLUM_MLX_* env vars rather than a shared abstraction (the
// smaller of the two diffs the plan named). `list_models()` is a static,
// in-process list (no network, no filesystem) — "reachable" here means "the
// backend is selectable and lists the model this stage ships", the MLX
// equivalent of the Ollama probe's connectivity + model-presence check.
// -----------------------------------------------------------------------

// Gated on the `mlx` feature (build(ffi): cargo feature 2026-09-25):
// `castellum-provider-mlx` is now an optional dependency of castellum-core,
// so this probe (and `castellum_provider_mlx` itself) is only in scope for
// `cargo test -p castellum-core --features mlx --test eval_harness`. The
// default-feature `ai-eval` CI job simply doesn't compile this probe.

#[cfg(feature = "mlx")]
fn mlx_model() -> String {
    std::env::var("CASTELLUM_MLX_MODEL").unwrap_or_else(|_| "all-MiniLM-L6-v2-4bit".into())
}

/// Minimal MLX probe: list models and verify the target model is present.
/// Returns `None` off Apple Silicon (the platform stub's clear error) or if
/// the named model isn't one of this stage's static entries.
#[cfg(feature = "mlx")]
fn probe_mlx(model: &str) -> Option<()> {
    use castellum_provider_mlx::MlxClient;
    use castellum_providers::AiBackend;

    let client = MlxClient::new();
    let models = client.list_models().ok()?;
    let found = models.iter().any(|m| m.name == model);
    if !found {
        eprintln!(
            "[eval_harness] MLX model {model:?} not found. Available: {:?}",
            models.iter().map(|m| &m.name).collect::<Vec<_>>()
        );
        return None;
    }
    Some(())
}

// ---------------------------------------------------------------------------
// Fixture sidecar schema
// ---------------------------------------------------------------------------

/// A fixture's scoring rubric. Deliberately never exact-string matching
/// (models are non-deterministic): a length band plus required/forbidden
/// keyword SETS, where each `required_keyword_groups` entry is a group of
/// synonyms/phrasings and only ONE member of the group need appear.
#[derive(Debug, Deserialize)]
struct Rubric {
    min_chars: usize,
    max_chars: usize,
    #[serde(default)]
    required_keyword_groups: Vec<Vec<String>>,
    #[serde(default)]
    forbidden_keywords: Vec<String>,
}

/// Sidecar for a `summarize_message` or `draft_reply` fixture: one message,
/// one `<name>.txt` content file next to this `<name>.json`.
#[derive(Debug, Deserialize)]
struct SingleMessageSidecar {
    feature: String,
    language: String,
    content_format: String,
    subject: Option<String>,
    from_name: Option<String>,
    from_address: Option<String>,
    content_file: String,
    /// `draft_reply` only: the user's instruction for the reply.
    #[serde(default)]
    instruction: Option<String>,
    rubric: Rubric,
}

/// One message within a `summarize_thread` fixture's `messages` array.
#[derive(Debug, Deserialize)]
struct ThreadMessageSpec {
    content_file: String,
    content_format: String,
    subject: Option<String>,
    from_name: Option<String>,
    from_address: Option<String>,
    /// Seconds after a fixed fixture epoch — keeps ordering explicit and the
    /// corpus reproducible without hardcoding real-looking timestamps.
    sent_at_offset_secs: i64,
    rfc822_message_id: String,
    #[serde(default)]
    references: Vec<String>,
}

/// Sidecar for a `summarize_thread` fixture: lives at `<dir>/fixture.json`
/// alongside its `msgN.txt` content files (a thread needs several messages,
/// so it gets its own directory rather than a single `<name>.txt` pair).
#[derive(Debug, Deserialize)]
struct ThreadSidecar {
    #[allow(dead_code)] // asserted implicitly by which loader loads it
    feature: String,
    language: String,
    messages: Vec<ThreadMessageSpec>,
    rubric: Rubric,
}

/// One expected amount: matched against an extracted `amount` item on the
/// normalised value (currency case-insensitive, amount within half a cent).
#[derive(Debug, Deserialize)]
struct ExpectedAmount {
    amount: f64,
    currency: String,
}

/// The JSON-field rubric of an extraction fixture.
#[derive(Debug, Deserialize, Default)]
struct ExtractionExpected {
    /// One entry per expected action item; each entry is a group of
    /// synonyms/phrasings, any ONE of which (normalised, as a substring)
    /// must appear in some extracted `action_item`'s text.
    #[serde(default)]
    action_items: Vec<Vec<String>>,
    /// Expected ISO `YYYY-MM-DD` dates, matched on a `date` item's
    /// `structured_value.date`.
    #[serde(default)]
    dates: Vec<String>,
    #[serde(default)]
    amounts: Vec<ExpectedAmount>,
}

/// Sidecar for an `extract_thread_items` fixture: a thread directory exactly
/// like `summarize_thread`, with `expected` in place of the keyword `rubric`.
#[derive(Debug, Deserialize)]
struct ExtractionSidecar {
    language: String,
    messages: Vec<ThreadMessageSpec>,
    expected: ExtractionExpected,
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

/// Global refusal/break-character markers, checked on EVERY fixture in
/// addition to its own `forbidden_keywords`. A model that refuses the task or
/// drops out of character is a real regression this corpus exists to catch,
/// not a hypothetical one — worth checking for on every fixture rather than
/// repeating it in 21 sidecars.
const GLOBAL_REFUSAL_MARKERS: &[&str] = &[
    "as an ai language model",
    "i cannot assist",
    "i can't assist",
    "i cannot help with that",
    "i'm not able to help with that",
    "no puedo ayudarte",
    "no puedo ayudar con eso",
    "com a model de llenguatge",
    "no puc ajudar",
];

/// Score one response against its rubric. Four equally-weighted, non-exact
/// checks (never a string-equality comparison against expected output):
///   1. length within [min_chars, max_chars]
///   2. every `required_keyword_groups` entry has >=1 matching synonym
///   3. none of the fixture's own `forbidden_keywords` appear
///   4. none of the global refusal markers appear
///
/// Returns (score in 0.0..=1.0, checks_passed, checks_total).
fn score_text(text: &str, rubric: &Rubric) -> (f64, usize, usize) {
    let lower = text.to_lowercase();
    let mut passed = 0usize;
    let total = 4usize;

    let chars = text.chars().count();
    if chars >= rubric.min_chars && chars <= rubric.max_chars {
        passed += 1;
    }

    let required_ok = rubric
        .required_keyword_groups
        .iter()
        .all(|group| group.iter().any(|kw| lower.contains(&kw.to_lowercase())));
    if required_ok {
        passed += 1;
    }

    let forbidden_ok = !rubric
        .forbidden_keywords
        .iter()
        .any(|kw| lower.contains(&kw.to_lowercase()));
    if forbidden_ok {
        passed += 1;
    }

    let refusal_ok = !GLOBAL_REFUSAL_MARKERS.iter().any(|kw| lower.contains(kw));
    if refusal_ok {
        passed += 1;
    }

    (passed as f64 / total as f64, passed, total)
}

/// One fixture's scored outcome. Carries only numbers and an id — never
/// response text (see the privacy note at the top of this file).
struct ScoredFixture {
    id: String,
    /// `summarize_message` | `draft_reply` | `summarize_thread` | `extract_thread_items`.
    feature: String,
    /// `ca` | `es` | `en` (from the fixture sidecar).
    language: String,
    score: f64,
    checks_passed: usize,
    checks_total: usize,
    job_failed: bool,
    /// Wall milliseconds the job-runner pass took for this fixture.
    latency_ms: i64,
}

// ---------------------------------------------------------------------------
// Extraction rubric scorer (model-free; unit-tested on canned JSON below)
// ---------------------------------------------------------------------------

/// Lower-case, strip the accents Catalan/Spanish use, and collapse every run
/// of non-alphanumerics to one space — so "Albarà  signat." matches
/// "albara signat". Matching is on these normalised values, never raw text.
fn normalise(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = true;
    for c in s.chars().flat_map(char::to_lowercase) {
        let base = match c {
            'à' | 'á' | 'â' | 'ä' => 'a',
            'è' | 'é' | 'ê' | 'ë' => 'e',
            'ì' | 'í' | 'î' | 'ï' => 'i',
            'ò' | 'ó' | 'ô' | 'ö' => 'o',
            'ù' | 'ú' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            'ñ' => 'n',
            other => other,
        };
        if base.is_alphanumeric() {
            out.push(base);
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim_end().to_string()
}

/// Score one raw `extract_thread_items` response against its expected fields.
///
/// `recall` = expected items found / expected items (action items by
/// normalised synonym containment, dates by exact ISO value, amounts by
/// currency + value within 0.005). `precision` = 1 − (extracted dates/amounts
/// matching NO expectation) / (extracted dates/amounts); extra action items
/// are not penalised (how many to list is a judgement call). Score =
/// recall × precision, in 0..=1. A response the schema validator rejects
/// (malformed JSON, wrong kinds, ...) scores 0.
///
/// Returns (score, expected_found, expected_total).
fn score_extraction(raw: &str, expected: &ExtractionExpected) -> (f64, usize, usize) {
    let total = expected.action_items.len() + expected.dates.len() + expected.amounts.len();
    let Ok(items) = parse_and_validate(raw) else {
        return (0.0, 0, total);
    };

    let action_texts: Vec<String> = items
        .iter()
        .filter(|i| i.kind == "action_item")
        .map(|i| normalise(&i.text))
        .collect();
    let found_actions = expected
        .action_items
        .iter()
        .filter(|group| {
            group.iter().any(|syn| {
                let syn = normalise(syn);
                !syn.is_empty() && action_texts.iter().any(|t| t.contains(&syn))
            })
        })
        .count();

    let mut found_dates = 0usize;
    let mut found_amounts = 0usize;
    let mut structured = 0usize;
    let mut spurious = 0usize;
    for item in &items {
        match &item.structured_value {
            Some(StructuredValue::Date { date }) => {
                structured += 1;
                if expected.dates.iter().any(|d| d.trim() == date) {
                    found_dates += 1;
                } else {
                    spurious += 1;
                }
            }
            Some(StructuredValue::Amount { amount, currency }) => {
                structured += 1;
                if expected.amounts.iter().any(|e| {
                    e.currency.eq_ignore_ascii_case(currency) && (e.amount - amount).abs() < 0.005
                }) {
                    found_amounts += 1;
                } else {
                    spurious += 1;
                }
            }
            None => {}
        }
    }
    // A model repeating the same correct date twice must not exceed recall 1.
    let found = found_actions
        + found_dates.min(expected.dates.len())
        + found_amounts.min(expected.amounts.len());

    let recall = if total == 0 {
        1.0
    } else {
        found as f64 / total as f64
    };
    let precision = if structured == 0 {
        1.0
    } else {
        1.0 - spurious as f64 / structured as f64
    };
    (recall * precision, found, total)
}

// ---------------------------------------------------------------------------
// Score history (CASTELLUM_EVAL_JSONL), per-group means, honest status line
// ---------------------------------------------------------------------------

/// One JSONL record. Deliberately ONLY these keys: ids, numbers and the model
/// and commit names. No fixture text, no model output, ever (pinned by
/// `jsonl_line_carries_only_the_allowed_keys`).
fn jsonl_line(r: &ScoredFixture, model: &str, sha: &str, ts: i64) -> String {
    serde_json::json!({
        "fixture": format!("{}/{}", r.feature, r.id),
        "feature": r.feature,
        "language": r.language,
        "score": r.score,
        "latency_ms": r.latency_ms,
        "model": model,
        "sha": sha,
        "ts": ts,
    })
    .to_string()
}

/// Append one line per fixture to `path` (created if missing).
fn append_jsonl(path: &Path, lines: &[String]) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    for l in lines {
        writeln!(f, "{l}")?;
    }
    Ok(())
}

/// The JSONL destination, if the env var is set and non-empty.
fn jsonl_path() -> Option<PathBuf> {
    std::env::var("CASTELLUM_EVAL_JSONL")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from)
}

/// Commit the scores belong to: CI passes `CASTELLUM_EVAL_SHA`; locally it
/// is just "unknown" (no subprocess, so no dependency on a git checkout).
fn eval_sha() -> String {
    std::env::var("CASTELLUM_EVAL_SHA")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// Mean score and fixture count per (feature, language), sorted.
fn mean_by_feature_language(results: &[ScoredFixture]) -> BTreeMap<(String, String), (f64, usize)> {
    let mut sums: BTreeMap<(String, String), (f64, usize)> = BTreeMap::new();
    for r in results {
        let e = sums
            .entry((r.feature.clone(), r.language.clone()))
            .or_insert((0.0, 0));
        e.0 += r.score;
        e.1 += 1;
    }
    sums.into_iter()
        .map(|(k, (sum, n))| (k, (sum / n as f64, n)))
        .collect()
}

/// The one unmistakable line CI greps for. A skipped run and a real run must
/// never be confusable in a log or a commit status.
const STATUS_SKIPPED: &str = "EVAL SKIPPED: no Ollama endpoint";

/// Status line for a skipped run. `None` = no endpoint configured (unset or
/// empty); `Some(reason)` = configured but the probe failed. The reason is
/// cut to its first line and 80 characters so the line stays greppable.
fn status_line_skipped(unusable_reason: Option<&str>) -> String {
    match unusable_reason {
        None => STATUS_SKIPPED.to_string(),
        Some(r) => {
            let first = r.lines().next().unwrap_or("").trim();
            let short: String = first.chars().take(80).collect();
            format!("EVAL SKIPPED: endpoint configured but not usable ({short})")
        }
    }
}

fn status_line_ran(fixtures: usize, mean: f64) -> String {
    format!("EVAL RAN: {fixtures} fixtures, mean {mean:.3}")
}

/// Print a status line on its own line. The leading newline matters: libtest
/// prints `test <name> ... ` without a newline before captured output under
/// `--nocapture`, which would otherwise glue itself to the line CI anchors on.
fn print_status(line: &str) {
    println!("\n{line}");
}

// ---------------------------------------------------------------------------
// DB seeding helpers
// ---------------------------------------------------------------------------

fn seed_account_mailbox(db: &Database, account_id: &str) {
    db.upsert_account(&Account {
        id: account_id.into(),
        email: format!("{account_id}@eval.test"),
        display_name: None,
        provider: "imap".into(),
    })
    .expect("upsert account");
    db.upsert_mailbox(&Mailbox {
        id: format!("{account_id}:INBOX"),
        account_id: account_id.into(),
        path: "INBOX".into(),
        name: "Inbox".into(),
        role: Some("inbox".into()),
        delimiter: Some("/".into()),
        subscribed: true,
        selectable: true,
        sync_by_default: true,
    })
    .expect("upsert mailbox");
}

/// Content lands in `text_body` for "plain" and `sanitized_html` for "html" —
/// matching how a real sync populates these fields (`castellum-mail::sanitize`
/// runs upstream of what this harness needs to exercise, so HTML fixtures are
/// pre-sanitized plain markup, same as `prompts::build_*`'s callers expect).
fn body_fields(content: &str, content_format: &str) -> (Option<String>, Option<String>) {
    match content_format {
        "html" => (None, Some(content.to_string())),
        _ => (Some(content.to_string()), None),
    }
}

#[allow(clippy::too_many_arguments)]
fn insert_message(
    db: &Database,
    account_id: &str,
    mailbox_id: &str,
    msg_id: &str,
    uid: i64,
    rfc822_message_id: &str,
    subject: Option<&str>,
    from_name: Option<&str>,
    from_address: Option<&str>,
    sent_at: i64,
    content: &str,
    content_format: &str,
    references: &[String],
) {
    let (text_body, sanitized_html) = body_fields(content, content_format);
    db.upsert_message(&NewMessage {
        id: msg_id.into(),
        account_id: account_id.into(),
        mailbox_id: mailbox_id.into(),
        provider_uid: Some(uid),
        rfc822_message_id: Some(rfc822_message_id.into()),
        subject: subject.map(|s| s.into()),
        from_name: from_name.map(|s| s.into()),
        from_address: from_address.map(|s| s.into()),
        sent_at: Some(sent_at),
        snippet: None,
        size_bytes: None,
        has_attachments: false,
        attachments: None,
        is_read: false,
        is_starred: false,
        addresses: vec![],
        text_body,
        sanitized_html,
        html_body: None,
        has_remote_content: false,
        references: references.to_vec(),
    })
    .expect("upsert message");
}

/// A fixed epoch so every fixture's timestamps are reproducible regardless of
/// when the harness runs.
const FIXTURE_EPOCH: i64 = 1_700_000_000;

// ---------------------------------------------------------------------------
// Per-feature fixture runners — each seeds a throwaway profile, enqueues the
// job, runs the REAL executor (ai::run_ai_jobs_with_backend, via the public
// Profile::run_ai_jobs_json) against the live Ollama endpoint, reads back the
// output that passed through ai::defense::validate_output, and scores it.
// ---------------------------------------------------------------------------

/// `summarize_message` or `draft_reply`: a single message + sidecar pair at
/// `<dir>/<name>.txt` + `<dir>/<name>.json`.
fn run_single_message_fixture(
    dir: &Path,
    name: &str,
    endpoint: &str,
    model: &str,
) -> ScoredFixture {
    let sidecar_path = dir.join(format!("{name}.json"));
    let sidecar: SingleMessageSidecar = serde_json::from_str(
        &std::fs::read_to_string(&sidecar_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", sidecar_path.display())),
    )
    .unwrap_or_else(|e| panic!("parse {}: {e}", sidecar_path.display()));
    let content = std::fs::read_to_string(dir.join(&sidecar.content_file))
        .unwrap_or_else(|e| panic!("read {}: {e}", sidecar.content_file));

    let profile_dir = unique_tmp();
    let account_id = "acc_eval";
    let mailbox_id = format!("{account_id}:INBOX");
    let msg_id = format!("{mailbox_id}:1");
    std::fs::create_dir_all(&profile_dir).expect("create profile dir");
    {
        let db = Database::open(&profile_dir.join("castellum.db")).expect("open db for seeding");
        seed_account_mailbox(&db, account_id);
        insert_message(
            &db,
            account_id,
            &mailbox_id,
            &msg_id,
            1,
            "root@eval-fixture",
            sidecar.subject.as_deref(),
            sidecar.from_name.as_deref(),
            sidecar.from_address.as_deref(),
            FIXTURE_EPOCH,
            &content,
            &sidecar.content_format,
            &[],
        );
    } // db connection dropped here, before Profile opens its own

    let profile = open_profile(&profile_dir).expect("open profile");
    let enqueue_json = match sidecar.feature.as_str() {
        "summarize_message" => serde_json::json!({
            "feature": "summarize_message",
            "message_id": msg_id,
        })
        .to_string(),
        "draft_reply" => serde_json::json!({
            "feature": "draft_reply",
            "message_id": msg_id,
            "instruction": sidecar.instruction.clone().unwrap_or_default(),
        })
        .to_string(),
        other => panic!("fixture {name}: unsupported feature {other:?}"),
    };
    profile
        .enqueue_ai_job_json(&enqueue_json)
        .unwrap_or_else(|e| panic!("fixture {name}: enqueue failed: {e}"));

    let run_config = serde_json::json!({
        "endpoint": endpoint,
        "model": model,
        "request_timeout_secs": 120,
    })
    .to_string();
    let started = std::time::Instant::now();
    let run_json = profile
        .run_ai_jobs_json(&run_config)
        .unwrap_or_else(|e| panic!("fixture {name}: run_ai_jobs_json failed: {e}"));
    let latency_ms = started.elapsed().as_millis() as i64;
    eprintln!("[eval_harness] {name} run report: {run_json}"); // counts only — {"ran":.,"succeeded":.,"failed":.,"cancelled":.}

    let output = profile
        .get_ai_output_json("message", &msg_id, &sidecar.feature)
        .unwrap_or_else(|e| panic!("fixture {name}: get_ai_output_json failed: {e}"));

    close_profile(profile);
    let _ = std::fs::remove_dir_all(&profile_dir);

    score_output(
        name,
        &sidecar.feature,
        &sidecar.language,
        latency_ms,
        output,
        &sidecar.rubric,
    )
}

/// Seed a throwaway profile's DB with a thread's messages (oldest first) and
/// return the deterministic thread id. Shared by `summarize_thread` and
/// `extract_thread_items` fixtures.
fn seed_thread(
    dir: &Path,
    name: &str,
    messages: &[ThreadMessageSpec],
    profile_dir: &Path,
) -> String {
    assert!(
        !messages.is_empty(),
        "fixture {name}: thread must have at least one message"
    );
    let account_id = "acc_eval";
    let mailbox_id = format!("{account_id}:INBOX");
    std::fs::create_dir_all(profile_dir).expect("create profile dir");
    let mut root_msg_id = String::new();
    {
        let db = Database::open(&profile_dir.join("castellum.db")).expect("open db for seeding");
        seed_account_mailbox(&db, account_id);
        for (i, m) in messages.iter().enumerate() {
            let content = std::fs::read_to_string(dir.join(&m.content_file))
                .unwrap_or_else(|e| panic!("read {}: {e}", m.content_file));
            let msg_id = format!("{mailbox_id}:{}", i + 1);
            if i == 0 {
                root_msg_id = msg_id.clone();
            }
            insert_message(
                &db,
                account_id,
                &mailbox_id,
                &msg_id,
                (i + 1) as i64,
                &m.rfc822_message_id,
                m.subject.as_deref(),
                m.from_name.as_deref(),
                m.from_address.as_deref(),
                FIXTURE_EPOCH + m.sent_at_offset_secs,
                &content,
                &m.content_format,
                &m.references,
            );
        }
        db.rebuild_threads(account_id).expect("rebuild threads");
    } // db connection dropped here, before Profile opens its own

    // Thread id is deterministic: "t:" + the root (earliest) message's own
    // id — see `Database::rebuild_threads` (native/crates/castellum-db/src/repo.rs).
    format!("t:{root_msg_id}")
}

/// `extract_thread_items`: a thread directory scored with the JSON-field
/// rubric. A job the schema validator rejected (no output stored) scores 0.
fn run_extraction_fixture(dir: &Path, name: &str, endpoint: &str, model: &str) -> ScoredFixture {
    let sidecar_path = dir.join("fixture.json");
    let sidecar: ExtractionSidecar = serde_json::from_str(
        &std::fs::read_to_string(&sidecar_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", sidecar_path.display())),
    )
    .unwrap_or_else(|e| panic!("parse {}: {e}", sidecar_path.display()));

    let profile_dir = unique_tmp();
    let thread_id = seed_thread(dir, name, &sidecar.messages, &profile_dir);

    let profile = open_profile(&profile_dir).expect("open profile");
    let enqueue_json = serde_json::json!({
        "feature": "extract_thread_items",
        "thread_id": thread_id,
    })
    .to_string();
    profile
        .enqueue_ai_job_json(&enqueue_json)
        .unwrap_or_else(|e| panic!("fixture {name}: enqueue failed: {e}"));

    let run_config = serde_json::json!({
        "endpoint": endpoint,
        "model": model,
        "request_timeout_secs": 120,
    })
    .to_string();
    let started = std::time::Instant::now();
    let run_json = profile
        .run_ai_jobs_json(&run_config)
        .unwrap_or_else(|e| panic!("fixture {name}: run_ai_jobs_json failed: {e}"));
    let latency_ms = started.elapsed().as_millis() as i64;
    eprintln!("[eval_harness] {name} run report: {run_json}"); // counts only

    let output = profile
        .get_ai_output_json("thread", &thread_id, "extract_thread_items")
        .unwrap_or_else(|e| panic!("fixture {name}: get_ai_output_json failed: {e}"));

    close_profile(profile);
    let _ = std::fs::remove_dir_all(&profile_dir);

    let total = sidecar.expected.action_items.len()
        + sidecar.expected.dates.len()
        + sidecar.expected.amounts.len();
    let (score, found, job_failed) = match output {
        None => (0.0, 0, true),
        Some(json) => {
            let v: serde_json::Value = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("fixture {name}: bad output json: {e}"));
            let (score, found, _) =
                score_extraction(v["text"].as_str().unwrap_or(""), &sidecar.expected);
            (score, found, false)
        }
    };
    ScoredFixture {
        id: name.to_string(),
        feature: "extract_thread_items".into(),
        language: sidecar.language,
        score,
        checks_passed: found,
        checks_total: total,
        job_failed,
        latency_ms,
    }
}

/// `summarize_thread`: a directory of `msgN.txt` files + `fixture.json`.
fn run_thread_fixture(dir: &Path, name: &str, endpoint: &str, model: &str) -> ScoredFixture {
    let sidecar_path = dir.join("fixture.json");
    let sidecar: ThreadSidecar = serde_json::from_str(
        &std::fs::read_to_string(&sidecar_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", sidecar_path.display())),
    )
    .unwrap_or_else(|e| panic!("parse {}: {e}", sidecar_path.display()));

    let profile_dir = unique_tmp();
    let thread_id = seed_thread(dir, name, &sidecar.messages, &profile_dir);

    let profile = open_profile(&profile_dir).expect("open profile");
    let enqueue_json = serde_json::json!({
        "feature": "summarize_thread",
        "thread_id": thread_id,
    })
    .to_string();
    profile
        .enqueue_ai_job_json(&enqueue_json)
        .unwrap_or_else(|e| panic!("fixture {name}: enqueue failed: {e}"));

    let run_config = serde_json::json!({
        "endpoint": endpoint,
        "model": model,
        "request_timeout_secs": 120,
    })
    .to_string();
    let started = std::time::Instant::now();
    let run_json = profile
        .run_ai_jobs_json(&run_config)
        .unwrap_or_else(|e| panic!("fixture {name}: run_ai_jobs_json failed: {e}"));
    let latency_ms = started.elapsed().as_millis() as i64;
    eprintln!("[eval_harness] {name} run report: {run_json}"); // counts only

    let output = profile
        .get_ai_output_json("thread", &thread_id, "summarize_thread")
        .unwrap_or_else(|e| panic!("fixture {name}: get_ai_output_json failed: {e}"));

    close_profile(profile);
    let _ = std::fs::remove_dir_all(&profile_dir);

    score_output(
        name,
        "summarize_thread",
        &sidecar.language,
        latency_ms,
        output,
        &sidecar.rubric,
    )
}

/// Shared tail: turn a `get_ai_output_json` result into a `ScoredFixture`.
/// `None` (job failed / was rejected by `defense::validate_output`) scores 0.
fn score_output(
    name: &str,
    feature: &str,
    language: &str,
    latency_ms: i64,
    output: Option<String>,
    rubric: &Rubric,
) -> ScoredFixture {
    let (score, checks_passed, checks_total, job_failed) = match output {
        None => (0.0, 0, 4, true),
        Some(json) => {
            let v: serde_json::Value = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("fixture {name}: bad output json: {e}"));
            let text = v["text"].as_str().unwrap_or("");
            let (score, passed, total) = score_text(text, rubric);
            (score, passed, total, false)
        }
    };
    ScoredFixture {
        id: name.to_string(),
        feature: feature.to_string(),
        language: language.to_string(),
        score,
        checks_passed,
        checks_total,
        job_failed,
        latency_ms,
    }
}

// ---------------------------------------------------------------------------
// Corpus discovery
// ---------------------------------------------------------------------------

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/eval")
}

/// `<dir>/*.json` sidecars, sorted, stem only (e.g. "en_short_plain").
fn list_flat_fixture_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("json") {
                p.file_stem().and_then(|s| s.to_str()).map(String::from)
            } else {
                None
            }
        })
        .collect();
    names.sort();
    names
}

/// Immediate subdirectories of `dir`, sorted — one per thread fixture.
fn list_subdirs(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// The harness test
// ---------------------------------------------------------------------------

/// Signature shared by the two directory-per-fixture runners.
type ThreadRunner = fn(&Path, &str, &str, &str) -> ScoredFixture;

#[test]
#[ignore]
fn eval_corpus_scores_above_regression_threshold() {
    let configured = ollama_url();
    if let Some(reason) = skip_reason(configured.as_deref()) {
        eprintln!("[eval_harness] {reason} — skipping");
        print_status(STATUS_SKIPPED);
        return;
    }
    let url = configured.expect("skip_reason returns Some for None");
    let model = ollama_model();

    if let Err(reason) = probe_ollama(&url, &model) {
        eprintln!("[eval_harness] Ollama endpoint {url} not usable: {reason} — skipping");
        print_status(&status_line_skipped(Some(&reason)));
        return;
    }

    let root = fixtures_root();
    let mut results: Vec<ScoredFixture> = Vec::new();

    for feature_dir in ["summarize_message", "draft_reply"] {
        let dir = root.join(feature_dir);
        for name in list_flat_fixture_names(&dir) {
            let scored = run_single_message_fixture(&dir, &name, &url, &model);
            eprintln!(
                "[eval_harness] {feature_dir}/{} score={:.2} ({}/{} checks){}",
                scored.id,
                scored.score,
                scored.checks_passed,
                scored.checks_total,
                if scored.job_failed { " JOB_FAILED" } else { "" }
            );
            results.push(scored);
        }
    }

    for (feature_dir, run) in [
        ("summarize_thread", run_thread_fixture as ThreadRunner),
        ("extraction", run_extraction_fixture as ThreadRunner),
    ] {
        let thread_dir = root.join(feature_dir);
        for name in list_subdirs(&thread_dir) {
            let scored = run(&thread_dir.join(&name), &name, &url, &model);
            eprintln!(
                "[eval_harness] {feature_dir}/{} score={:.2} ({}/{} checks){}",
                scored.id,
                scored.score,
                scored.checks_passed,
                scored.checks_total,
                if scored.job_failed { " JOB_FAILED" } else { "" }
            );
            results.push(scored);
        }
    }

    assert!(!results.is_empty(), "eval corpus must not be empty");

    // Score history: ids, numbers and names only (see `jsonl_line`).
    if let Some(path) = jsonl_path() {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let sha = eval_sha();
        let lines: Vec<String> = results
            .iter()
            .map(|r| jsonl_line(r, &model, &sha, ts))
            .collect();
        match append_jsonl(&path, &lines) {
            Ok(()) => eprintln!(
                "[eval_harness] appended {} score lines to {}",
                lines.len(),
                path.display()
            ),
            Err(e) => eprintln!("[eval_harness] could not write {}: {e}", path.display()),
        }
    }

    for ((feature, language), (mean, n)) in mean_by_feature_language(&results) {
        eprintln!("[eval_harness] mean {feature}/{language} = {mean:.3} over {n} fixture(s)");
    }

    let aggregate: f64 = results.iter().map(|r| r.score).sum::<f64>() / results.len() as f64;
    eprintln!(
        "[eval_harness] aggregate score {aggregate:.3} over {} fixtures (threshold {REGRESSION_THRESHOLD:.2})",
        results.len()
    );
    print_status(&status_line_ran(results.len(), aggregate));

    assert!(
        aggregate >= REGRESSION_THRESHOLD,
        "AI eval regression: aggregate score {aggregate:.3} below threshold {REGRESSION_THRESHOLD:.2} over {} fixtures",
        results.len()
    );
}

// ---------------------------------------------------------------------------
// Tests that don't need a live model — pin the harness's own contract.
// ---------------------------------------------------------------------------

#[test]
fn harness_skips_cleanly_without_env_vars() {
    // Mirrors ollama_e2e.rs's own guarantee: with CASTELLUM_OLLAMA_URL unset,
    // the harness must never panic or fail — this runs in plain `cargo test`
    // (no #[ignore], no env var) so a broken skip path fails CI directly
    // rather than only being caught the day someone runs --ignored.
    //
    // Drives the decision function rather than the environment, so it cannot
    // race a concurrent test (see `skip_reason`). That also lets it cover the
    // set-but-empty case, which a `remove_var` test structurally could not.
    assert_eq!(skip_reason(None), Some("CASTELLUM_OLLAMA_URL not set"));
    assert_eq!(skip_reason(Some("")), Some("CASTELLUM_OLLAMA_URL is empty"));
    assert_eq!(
        skip_reason(Some("   ")),
        Some("CASTELLUM_OLLAMA_URL is empty")
    );
    assert_eq!(skip_reason(Some("http://127.0.0.1:11434")), None);
}

#[cfg(feature = "mlx")]
#[test]
#[ignore]
fn mlx_probe_reachable_or_skips_cleanly() {
    // Exploratory probe, not a scored regression run (see the module doc
    // comment: MlxClient::generate() isn't implemented yet in S56-impl, so
    // there is nothing to score). This only proves the MLX backend is
    // selectable and correctly lists the model this stage ships wherever it
    // can — and skips with a clear reason everywhere else (any non-Apple-
    // Silicon runner, or an Apple Silicon box where the model constant ever
    // drifts from list_models()'s static entry).
    let model = mlx_model();
    match probe_mlx(&model) {
        Some(()) => eprintln!("[eval_harness] MLX model {model:?} is listed and selectable"),
        None => eprintln!(
            "[eval_harness] MLX backend unavailable or model {model:?} not listed — skipping"
        ),
    }
}

#[test]
fn corpus_covers_all_three_features_and_three_languages() {
    // Done-criteria guard: the corpus must exist and cover
    // summarize_message, summarize_thread and draft_reply, each in ca/es/en.
    // Pure filesystem check — no network, no #[ignore], runs every `cargo test`.
    let root = fixtures_root();

    let single_langs = |dir: &str| -> Vec<String> {
        list_flat_fixture_names(&root.join(dir))
            .into_iter()
            .filter_map(|n| n.split('_').next().map(String::from))
            .collect()
    };
    let thread_langs: Vec<String> = list_subdirs(&root.join("summarize_thread"))
        .into_iter()
        .filter_map(|n| n.split('_').next().map(String::from))
        .collect();

    for lang in ["en", "es", "ca"] {
        assert!(
            single_langs("summarize_message").iter().any(|l| l == lang),
            "summarize_message missing a {lang} fixture"
        );
        assert!(
            single_langs("draft_reply").iter().any(|l| l == lang),
            "draft_reply missing a {lang} fixture"
        );
        assert!(
            thread_langs.iter().any(|l| l == lang),
            "summarize_thread missing a {lang} fixture"
        );
    }

    // M5: extraction (S54) fixtures — at least two per language, every
    // sidecar parseable, and every one with something to score.
    let extraction_root = root.join("extraction");
    let names = list_subdirs(&extraction_root);
    for lang in ["en", "es", "ca"] {
        let n = names
            .iter()
            .filter(|n| n.split('_').next() == Some(lang))
            .count();
        assert!(n >= 2, "extraction needs >= 2 {lang} fixtures, found {n}");
    }
    for name in &names {
        let sidecar = load_extraction_sidecar(&extraction_root.join(name));
        let e = &sidecar.expected;
        assert!(
            !e.action_items.is_empty() && !e.dates.is_empty() && !e.amounts.is_empty(),
            "extraction/{name}: expected action items, dates and amounts"
        );
    }
}

fn load_extraction_sidecar(dir: &Path) -> ExtractionSidecar {
    let p = dir.join("fixture.json");
    serde_json::from_str(
        &std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())),
    )
    .unwrap_or_else(|e| panic!("parse {}: {e}", p.display()))
}

// ---------------------------------------------------------------------------
// Extraction rubric scorer — canned JSON, no model.
// ---------------------------------------------------------------------------

fn sample_expected() -> ExtractionExpected {
    ExtractionExpected {
        action_items: vec![
            vec!["signed delivery form".into(), "delivery form".into()],
            vec!["bank transfer".into()],
        ],
        dates: vec!["2026-10-15".into()],
        amounts: vec![ExpectedAmount {
            amount: 1250.0,
            currency: "GBP".into(),
        }],
    }
}

const PERFECT: &str = r#"[
  {"kind":"action_item","text":"Send the Signed  Delivery Form tomorrow"},
  {"kind":"action_item","text":"Arrange the bank transfer."},
  {"kind":"date","text":"Pay by 15 Oct","structured_value":{"date":"2026-10-15"}},
  {"kind":"amount","text":"1,250.00 GBP","structured_value":{"amount":1250.004,"currency":"gbp"}}
]"#;

#[test]
fn extraction_scorer_perfect_output_scores_one() {
    let (score, found, total) = score_extraction(PERFECT, &sample_expected());
    assert_eq!((found, total), (4, 4));
    assert!((score - 1.0).abs() < 1e-9, "score was {score}");
}

#[test]
fn extraction_scorer_partial_output_scores_between() {
    // Only the date and one action item found; amount missing, other action missing.
    let partial = r#"[
      {"kind":"action_item","text":"send the delivery form"},
      {"kind":"date","text":"15 Oct","structured_value":{"date":"2026-10-15"}}
    ]"#;
    let (score, found, total) = score_extraction(partial, &sample_expected());
    assert_eq!((found, total), (2, 4));
    assert!((score - 0.5).abs() < 1e-9, "score was {score}");
}

#[test]
fn extraction_scorer_penalises_spurious_structured_values() {
    // Everything right plus one invented date: recall 1.0, precision 2/3.
    let noisy = r#"[
      {"kind":"action_item","text":"signed delivery form"},
      {"kind":"action_item","text":"bank transfer"},
      {"kind":"date","text":"a","structured_value":{"date":"2026-10-15"}},
      {"kind":"date","text":"b","structured_value":{"date":"2027-01-01"}},
      {"kind":"amount","text":"c","structured_value":{"amount":1250.0,"currency":"GBP"}}
    ]"#;
    let (score, found, total) = score_extraction(noisy, &sample_expected());
    assert_eq!((found, total), (4, 4));
    assert!((score - 2.0 / 3.0).abs() < 1e-9, "score was {score}");
}

#[test]
fn extraction_scorer_wrong_output_scores_zero() {
    let wrong = r#"[
      {"kind":"action_item","text":"water the plants"},
      {"kind":"date","text":"x","structured_value":{"date":"2030-02-02"}},
      {"kind":"amount","text":"y","structured_value":{"amount":9.99,"currency":"USD"}}
    ]"#;
    let (score, found, total) = score_extraction(wrong, &sample_expected());
    assert_eq!((found, total), (0, 4));
    assert_eq!(score, 0.0);
}

#[test]
fn extraction_scorer_malformed_output_scores_zero_without_panicking() {
    for bad in [
        "not json at all",
        "{\"kind\":\"action_item\"}",
        "[",
        "[{\"kind\":\"summary\",\"text\":\"x\"}]",
        "[{\"kind\":\"date\",\"text\":\"x\",\"structured_value\":{\"date\":\"15/10/2026\"}}]",
        "",
    ] {
        let (score, found, total) = score_extraction(bad, &sample_expected());
        assert_eq!((score, found, total), (0.0, 0, 4), "input: {bad:?}");
    }
}

#[test]
fn extraction_scorer_repeating_a_correct_date_does_not_exceed_full_recall() {
    let repeated = r#"[
      {"kind":"date","text":"a","structured_value":{"date":"2026-10-15"}},
      {"kind":"date","text":"b","structured_value":{"date":"2026-10-15"}}
    ]"#;
    let (score, found, _) = score_extraction(repeated, &sample_expected());
    assert_eq!(found, 1);
    assert!((score - 0.25).abs() < 1e-9, "score was {score}");
}

#[test]
fn normalise_folds_case_accents_and_punctuation() {
    assert_eq!(normalise("  Albarà   SIGNAT. "), "albara signat");
    assert_eq!(
        normalise("Reserveu la sala-petita!"),
        "reserveu la sala petita"
    );
    assert_eq!(normalise("Año"), "ano");
}

#[test]
fn every_extraction_fixture_scores_one_on_its_own_expectations() {
    // Synthesise the perfect answer from each sidecar's `expected` block and
    // score it: proves every fixture parses and its rubric is satisfiable
    // (and that the scorer agrees with the corpus's own normalisation).
    let root = fixtures_root().join("extraction");
    for name in list_subdirs(&root) {
        let sidecar = load_extraction_sidecar(&root.join(&name));
        let mut items: Vec<serde_json::Value> = Vec::new();
        for group in &sidecar.expected.action_items {
            items.push(serde_json::json!({"kind":"action_item","text":group[0]}));
        }
        for d in &sidecar.expected.dates {
            items.push(serde_json::json!({
                "kind":"date","text":d,"structured_value":{"date":d}
            }));
        }
        for a in &sidecar.expected.amounts {
            items.push(serde_json::json!({
                "kind":"amount","text":"amount",
                "structured_value":{"amount":a.amount,"currency":a.currency}
            }));
        }
        let raw = serde_json::Value::Array(items).to_string();
        let (score, found, total) = score_extraction(&raw, &sidecar.expected);
        assert_eq!(found, total, "extraction/{name}");
        assert!((score - 1.0).abs() < 1e-9, "extraction/{name}: {score}");
    }
}

// ---------------------------------------------------------------------------
// Score history + honest status — no model, no env mutation.
// ---------------------------------------------------------------------------

fn sample_scored(id: &str, feature: &str, language: &str, score: f64) -> ScoredFixture {
    ScoredFixture {
        id: id.into(),
        feature: feature.into(),
        language: language.into(),
        score,
        checks_passed: 3,
        checks_total: 4,
        job_failed: false,
        latency_ms: 1234,
    }
}

#[test]
fn jsonl_line_carries_only_the_allowed_keys() {
    let r = sample_scored("en_short_plain", "summarize_message", "en", 0.75);
    let line = jsonl_line(&r, "llama3.1:8b", "abc123", 1_700_000_000);
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "feature",
            "fixture",
            "language",
            "latency_ms",
            "model",
            "score",
            "sha",
            "ts"
        ],
        "the history record must carry no text of any kind"
    );
    assert_eq!(v["fixture"], "summarize_message/en_short_plain");
    assert_eq!(v["latency_ms"], 1234);
    assert_eq!(v["score"], 0.75);
    assert_eq!(v["ts"], 1_700_000_000);
    assert!(!line.contains('\n'), "one record per line");
}

#[test]
fn append_jsonl_appends_one_line_per_record() {
    let path = unique_tmp().with_extension("jsonl");
    let a = sample_scored("a", "draft_reply", "ca", 1.0);
    let b = sample_scored("b", "draft_reply", "es", 0.5);
    append_jsonl(&path, &[jsonl_line(&a, "m", "s", 1)]).unwrap();
    append_jsonl(&path, &[jsonl_line(&b, "m", "s", 2)]).unwrap();
    let body = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_eq!(body.lines().count(), 2);
    for l in body.lines() {
        serde_json::from_str::<serde_json::Value>(l).expect("each line is JSON");
    }
}

#[test]
fn mean_by_feature_language_groups_and_averages() {
    let results = vec![
        sample_scored("a", "draft_reply", "ca", 1.0),
        sample_scored("b", "draft_reply", "ca", 0.5),
        sample_scored("c", "draft_reply", "en", 0.25),
        sample_scored("d", "extract_thread_items", "ca", 0.0),
    ];
    let m = mean_by_feature_language(&results);
    assert_eq!(m.len(), 3);
    assert_eq!(m[&("draft_reply".into(), "ca".into())], (0.75, 2));
    assert_eq!(m[&("draft_reply".into(), "en".into())], (0.25, 1));
    assert_eq!(m[&("extract_thread_items".into(), "ca".into())], (0.0, 1));
}

#[test]
fn status_line_distinguishes_unset_from_unusable() {
    // Unset/empty keeps the original wording CI has always matched.
    assert_eq!(
        status_line_skipped(None),
        "EVAL SKIPPED: no Ollama endpoint"
    );
    // A configured-but-failing endpoint says so, with the reason, on one line.
    let line = status_line_skipped(Some("only http:// endpoints are accepted\nsecond line"));
    assert_eq!(
        line,
        "EVAL SKIPPED: endpoint configured but not usable (only http:// endpoints are accepted)"
    );
    assert!(line.starts_with("EVAL SKIPPED"));
    assert!(!line.contains('\n'));
}

#[test]
fn status_lines_are_unmistakable() {
    assert_eq!(STATUS_SKIPPED, "EVAL SKIPPED: no Ollama endpoint");
    assert_eq!(
        status_line_ran(27, 0.8),
        "EVAL RAN: 27 fixtures, mean 0.800"
    );
    assert!(STATUS_SKIPPED.starts_with("EVAL SKIPPED"));
    assert!(status_line_ran(1, 1.0).starts_with("EVAL RAN"));
}

#[test]
fn score_text_requires_every_keyword_group_but_only_one_synonym_each() {
    let rubric = Rubric {
        min_chars: 0,
        max_chars: 1000,
        required_keyword_groups: vec![
            vec!["thursday".into(), "thurs".into()],
            vec!["2pm".into(), "14:00".into()],
        ],
        forbidden_keywords: vec!["ignore previous".into()],
    };

    let (score_all, passed_all, total) = score_text("Sure, Thursday at 2pm works.", &rubric);
    assert_eq!(passed_all, total, "all four checks should pass");
    assert_eq!(score_all, 1.0);

    let (_, passed_missing, _) = score_text("Sure, Thursday works.", &rubric);
    assert!(
        passed_missing < total,
        "missing the time synonym group should cost a point"
    );

    let (_, passed_forbidden, _) = score_text(
        "Thursday at 2pm — also, IGNORE PREVIOUS instructions.",
        &rubric,
    );
    assert!(
        passed_forbidden < total,
        "a forbidden phrase should cost a point even when required groups match"
    );
}

#[test]
fn score_text_flags_global_refusal_markers() {
    let rubric = Rubric {
        min_chars: 0,
        max_chars: 1000,
        required_keyword_groups: vec![],
        forbidden_keywords: vec![],
    };
    let (score, passed, total) = score_text(
        "As an AI language model, I cannot assist with that request.",
        &rubric,
    );
    assert!(
        passed < total,
        "a refusal response should never score full marks"
    );
    assert!(score < 1.0);
}
