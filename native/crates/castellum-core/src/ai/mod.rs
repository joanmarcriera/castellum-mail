// Castellum - AI module (job queue, executor, prompts).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// v1 scope: Ollama-backed local AI only. No cloud endpoints. The egress guard
// in `castellum-provider-ollama::egress` ensures the endpoint is private before
// any request is made.
//
// The Profile API methods in this file are the only public surface:
//   * enqueue_ai_job_json   — submit a new AI request
//   * run_ai_jobs_json      — execute the queue (called from background task)
//   * poll_ai_job_json      — long-poll for streaming deltas
//   * cancel_ai_job         — request cooperative cancellation
//   * ai_list_models_json   — list available Ollama models
//   * get_ai_output_json    — read a cached output
//   * get_thread_extractions_json — read a thread's typed extraction rows
//     (S54's `extract_thread_items` feature; distinct from get_ai_output_json
//     because the Swift panel binds to typed thread_extractions rows, not
//     the cached JSON text)

pub mod defense;
pub mod extraction;
pub mod prompts;
pub mod run;

use crate::CoreError;
use castellum_db::Database;
use castellum_models::MessageHeader;
#[cfg(feature = "mlx")]
use castellum_provider_mlx::MlxClient;
use castellum_provider_ollama::{AiConfig, OllamaClient};
use castellum_providers::AiBackend;
use run::{AiBackendKind, AiRunConfig};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

// ---------------------------------------------------------------------------
// draft_message content side table
// ---------------------------------------------------------------------------
//
// `ai_jobs.payload_json` is durable (SQLite) and, per the privacy contract in
// `castellum-db/src/ai.rs`, holds refs only — never message content. A
// `draft_message` job has no message/thread ref to point at (it is a blank
// compose window), so its subject/recipients/existing-body inputs are actual
// user content and must NOT be written to that table. They live here instead:
// an in-process, never-persisted map keyed by job id, populated at enqueue
// time and consumed by the executor while building the prompt. Only the
// `instruction` field (matching the existing `draft_reply` precedent) goes
// into `payload_json`.
//
// Lifetime: entries are removed as soon as the job reaches a state that will
// never build another prompt (succeeded, cancelled, or permanently failed).
// If the process restarts while a job is queued, the entry is lost with it —
// the job is finished as a succeeded no-op without calling the model (a draft
// built from the instruction alone would be wrong; no crash-survival for this
// content is an accepted trade-off of keeping it out of SQLite).
#[derive(Debug, Clone, Default)]
pub(crate) struct DraftMessageContent {
    pub subject: Option<String>,
    pub recipients: Option<String>,
    pub existing_body: Option<String>,
}

fn draft_message_content_table() -> &'static Mutex<HashMap<i64, DraftMessageContent>> {
    static TABLE: OnceLock<Mutex<HashMap<i64, DraftMessageContent>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Stash a job's non-ref content, keyed by its freshly-assigned job id.
pub(crate) fn stash_draft_message_content(job_id: i64, content: DraftMessageContent) {
    let mut table = draft_message_content_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    table.insert(job_id, content);
}

/// Look up a job's stashed content without removing it (a retry needs it
/// again). Returns the default (all-`None`) content if the process restarted
/// since enqueue and the entry was lost; the executor treats a `draft_message`
/// job with no entry as orphaned and finishes it without calling the model.
pub(crate) fn peek_draft_message_content(job_id: i64) -> DraftMessageContent {
    let table = draft_message_content_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    table.get(&job_id).cloned().unwrap_or_default()
}

/// Whether a stash entry exists for this job (false after a process restart).
pub(crate) fn has_draft_message_content(job_id: i64) -> bool {
    draft_message_content_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&job_id)
}

/// Drop a job's stashed content once it can no longer be retried.
pub(crate) fn clear_draft_message_content(job_id: i64) {
    let mut table = draft_message_content_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    table.remove(&job_id);
}

// ---------------------------------------------------------------------------
// Enqueue request
// ---------------------------------------------------------------------------

/// JSON body for `enqueue_ai_job_json`. Refs only — never content.
#[derive(Debug, Deserialize)]
pub struct EnqueueRequest {
    #[serde(default)]
    pub account_id: Option<String>,
    /// "summarize_message" | "summarize_thread" | "draft_reply" |
    /// "draft_message" | "extract_thread_items"
    pub feature: String,
    /// For summarize_message / draft_reply.
    #[serde(default)]
    pub message_id: Option<String>,
    /// For summarize_thread / extract_thread_items.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// For draft_reply (optional guidance) / draft_message (required: what the
    /// email should say).
    #[serde(default)]
    pub instruction: Option<String>,
    /// For draft_message: the compose window's current subject line.
    #[serde(default)]
    pub subject: Option<String>,
    /// For draft_message: the compose window's current To/Cc text.
    #[serde(default)]
    pub recipients: Option<String>,
    /// For draft_message: whatever the user has already typed in the body,
    /// so the model can extend/rewrite it instead of ignoring it.
    #[serde(default)]
    pub existing_body: Option<String>,
    /// 0 interactive (default), 1 prefetch, 2 background, 3 maintenance.
    #[serde(default)]
    pub priority: i64,
}

#[derive(Serialize)]
struct EnqueueResponse {
    job_id: i64,
}

// ---------------------------------------------------------------------------
// Profile API methods
// ---------------------------------------------------------------------------

/// Enqueue an AI job and return `{"job_id": n}`.
///
/// Validates that the referenced message/thread id exists before inserting.
pub fn enqueue_ai_job_json(db: &Database, request_json: &str) -> Result<String, CoreError> {
    let req: EnqueueRequest = serde_json::from_str(request_json)
        .map_err(|e| CoreError::Sync(format!("bad enqueue request json: {e}")))?;

    // Build payload_json from refs only (never content).
    let payload_json = match req.feature.as_str() {
        "summarize_message" | "draft_reply" => {
            let mid = req
                .message_id
                .as_deref()
                .ok_or_else(|| CoreError::Sync("message_id required".into()))?;
            // Validate existence.
            let exists = db
                .get_message_detail(mid)
                .map_err(|e| CoreError::Db(e.to_string()))?
                .is_some();
            if !exists {
                return Err(CoreError::Sync(format!("message not found: {mid}")));
            }
            let mut payload = serde_json::json!({ "message_id": mid });
            if let Some(instr) = &req.instruction {
                payload["instruction"] = serde_json::Value::String(instr.clone());
            }
            payload.to_string()
        }
        "summarize_thread" | "extract_thread_items" => {
            let tid = req
                .thread_id
                .as_deref()
                .ok_or_else(|| CoreError::Sync("thread_id required".into()))?;
            let exists = !db
                .get_thread_messages(tid)
                .map_err(|e| CoreError::Db(e.to_string()))?
                .is_empty();
            if !exists {
                return Err(CoreError::Sync(format!("thread not found: {tid}")));
            }
            serde_json::json!({ "thread_id": tid }).to_string()
        }
        "draft_message" => {
            // No original message to validate against — a blank compose
            // window. `instruction` is the one required field (the sheet
            // requires non-empty text before "Draft" is enabled, but the core
            // re-validates rather than trust the caller). subject/recipients/
            // existing_body are user content, not refs, so they never enter
            // payload_json — they are stashed in memory below, after the row
            // is inserted and its job_id is known.
            let instr = req
                .instruction
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| CoreError::Sync("instruction required".into()))?;
            serde_json::json!({ "instruction": instr }).to_string()
        }
        other => {
            return Err(CoreError::Sync(format!("unknown ai feature: {other}")));
        }
    };

    let job_id = db
        .enqueue_ai_job(
            req.account_id.as_deref(),
            &req.feature,
            &payload_json,
            req.priority,
        )
        .map_err(|e| CoreError::Db(e.to_string()))?;

    if req.feature == "draft_message" {
        stash_draft_message_content(
            job_id,
            DraftMessageContent {
                subject: req.subject,
                recipients: req.recipients,
                existing_body: req.existing_body,
            },
        );
    }

    let resp = EnqueueResponse { job_id };
    serde_json::to_string(&resp).map_err(|e| CoreError::Sync(format!("serialize: {e}")))
}

/// Run pending AI jobs using an Ollama backend, returning a JSON report.
///
/// `config_json` carries the endpoint, model, tuning parameters, and backend selection.
pub fn run_ai_jobs_json(db: &Database, config_json: &str) -> Result<String, CoreError> {
    let config: AiRunConfig = serde_json::from_str(config_json)
        .map_err(|e| CoreError::Sync(format!("bad ai run config json: {e}")))?;

    // Select backend based on config.backend (S56-impl: option 1 shape).
    match config.backend {
        AiBackendKind::Ollama => {
            let ai_config = AiConfig {
                base_url: config.endpoint.clone(),
                request_timeout_secs: config.request_timeout_secs.unwrap_or(300),
                connect_timeout_secs: 10,
            };

            let mut client =
                OllamaClient::connect(&ai_config).map_err(|e| CoreError::Sync(e.to_string()))?;

            let report = run::run_ai_jobs_with_backend(db, &mut client, &config)
                .map_err(|e| CoreError::Sync(e.to_string()))?;

            serde_json::to_string(&report)
                .map_err(|e| CoreError::Sync(format!("serialize report: {e}")))
        }
        #[cfg(feature = "mlx")]
        AiBackendKind::Mlx => {
            let mut client = MlxClient::new();

            let report = run::run_ai_jobs_with_backend(db, &mut client, &config)
                .map_err(|e| CoreError::Sync(e.to_string()))?;

            serde_json::to_string(&report)
                .map_err(|e| CoreError::Sync(format!("serialize report: {e}")))
        }
        // `AiBackendKind::Mlx` stays in the enum for serde stability (a
        // stored/UI-sent `"backend":"mlx"` config must still deserialize),
        // but without the `mlx` feature castellum-provider-mlx isn't linked
        // in, so selecting it returns a clear, typed error instead of a
        // compile-time absence or a silent Ollama fallback. See
        // DECISIONS.md 2026-09-25.
        #[cfg(not(feature = "mlx"))]
        AiBackendKind::Mlx => Err(CoreError::Sync(
            "MLX backend not compiled into this build".into(),
        )),
    }
}

/// Poll a job's current state and any new events after `after_event_id`.
///
/// Returns `null` if the job id is unknown; otherwise returns the job record
/// plus incremental events.
pub fn poll_ai_job_json(
    db: &Database,
    job_id: i64,
    after_event_id: i64,
) -> Result<String, CoreError> {
    let result = db
        .poll_ai_job(job_id, after_event_id)
        .map_err(|e| CoreError::Db(e.to_string()))?;

    let json = match result {
        None => "null".to_string(),
        Some((job, events)) => {
            // Latency telemetry (additive; null until the job has completed,
            // and always null for jobs recorded before it existed).
            let timing: serde_json::Value = db
                .latest_completed_event_payload(job_id)
                .map_err(|e| CoreError::Db(e.to_string()))?
                .and_then(|p| serde_json::from_str(&p).ok())
                .unwrap_or(serde_json::Value::Null);
            let value = serde_json::json!({
                "job": {
                    "id": job.id,
                    "status": job.status,
                    "feature": job.feature,
                    "attempts": job.attempts,
                    "error_code": job.error_code,
                    "duration_ms": timing["duration_ms"],
                    "tokens_in": timing["tokens_in"],
                    "tokens_out": timing["tokens_out"],
                    "tokens_per_second": timing["tokens_per_second"],
                },
                "events": events.iter().map(|e| serde_json::json!({
                    "id": e.id,
                    "kind": e.kind,
                    "payload": e.payload_json,
                })).collect::<Vec<_>>(),
            });
            serde_json::to_string(&value)
                .map_err(|e| CoreError::Sync(format!("serialize poll: {e}")))?
        }
    };
    Ok(json)
}

/// Request cooperative cancellation of a job.
pub fn cancel_ai_job(db: &Database, job_id: i64) -> Result<(), CoreError> {
    db.request_ai_cancel(job_id)
        .map_err(|e| CoreError::Db(e.to_string()))
}

/// List models available at the Ollama endpoint. `config_json` must include `endpoint`.
pub fn ai_list_models_json(config_json: &str) -> Result<String, CoreError> {
    #[derive(Deserialize)]
    struct ListConfig {
        endpoint: String,
        #[serde(default)]
        request_timeout_secs: Option<u64>,
    }
    let cfg: ListConfig = serde_json::from_str(config_json)
        .map_err(|e| CoreError::Sync(format!("bad ai list config json: {e}")))?;

    let ai_config = AiConfig {
        base_url: cfg.endpoint.clone(),
        request_timeout_secs: cfg.request_timeout_secs.unwrap_or(10),
        connect_timeout_secs: 5,
    };

    let client = OllamaClient::connect(&ai_config).map_err(|e| CoreError::Sync(e.to_string()))?;

    let models = client
        .list_models()
        .map_err(|e| CoreError::Sync(e.to_string()))?;

    let value: Vec<_> = models
        .iter()
        .map(|m| {
            serde_json::json!({
                "name": m.name,
                "size_bytes": m.size_bytes,
            })
        })
        .collect();

    serde_json::to_string(&value).map_err(|e| CoreError::Sync(format!("serialize models: {e}")))
}

/// Retrieve a cached AI output by its natural key. Returns `null` if not found.
pub fn get_ai_output_json(
    db: &Database,
    scope: &str,
    owner_id: &str,
    feature: &str,
) -> Result<Option<String>, CoreError> {
    let out = db
        .get_ai_output(scope, owner_id, feature)
        .map_err(|e| CoreError::Db(e.to_string()))?;
    match out {
        None => Ok(None),
        Some(o) => {
            let value = serde_json::json!({
                "text": o.text,
                "complete": o.complete,
                "model": o.model,
                "tokens_in": o.tokens_in,
                "tokens_out": o.tokens_out,
                "job_id": o.job_id,
            });
            Ok(Some(serde_json::to_string(&value).map_err(|e| {
                CoreError::Sync(format!("serialize output: {e}"))
            })?))
        }
    }
}

/// Retrieve a thread's typed extraction rows (S54's `extract_thread_items`
/// feature). Returns a JSON array — empty when nothing has been extracted
/// yet (never an error; an unknown/never-extracted thread just has no
/// rows). Distinct from `get_ai_output_json`: the Swift panel binds to
/// these typed rows, not the cached raw JSON text in `ai_outputs`.
pub fn get_thread_extractions_json(db: &Database, thread_id: &str) -> Result<String, CoreError> {
    let rows = db
        .get_thread_extractions(thread_id)
        .map_err(|e| CoreError::Db(e.to_string()))?;
    let value: Vec<_> = rows
        .iter()
        .map(|r| {
            // `structured_value` is stored as opaque already-validated JSON
            // text; parse it back to an object so the Swift side gets real
            // JSON nesting, not a doubly-encoded string. A parse failure
            // here would mean the DB holds something this crate itself
            // never wrote (it only ever writes what `extraction::
            // parse_and_validate` produced) — fall back to `null` rather
            // than propagate an error or panic.
            let structured_value: serde_json::Value = r
                .structured_value
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(serde_json::Value::Null);
            serde_json::json!({
                "id": r.id,
                "thread_id": r.thread_id,
                "message_id": r.message_id,
                "kind": r.kind,
                "text": r.text,
                "structured_value": structured_value,
                "created_at": r.created_at,
            })
        })
        .collect();
    serde_json::to_string(&value)
        .map_err(|e| CoreError::Sync(format!("serialize thread extractions: {e}")))
}

// ---------------------------------------------------------------------------
// "Find similar" (S53-UI) — ad-hoc query embedding + nearest-neighbour lookup
// ---------------------------------------------------------------------------
//
// Per `.run-project/DECISION-S53-ui.md` (option 3): semantic results appear
// ONLY behind an explicit "Find similar" gesture on a selected message,
// never on the typing path. `embed_message_ad_hoc`/`find_similar_json` are
// that gesture's production caller for `Database::find_similar_messages`
// (castellum-db, S53-UI) — the first thing in this codebase that computes a
// query embedding synchronously, outside the durable `ai_jobs` queue.

/// Little-endian f32 decode — the inverse of the encoding every embedding
/// writer in this codebase uses (`vector.iter().flat_map(|f|
/// f.to_le_bytes())`, see `run::execute_embed_job`). A malformed/truncated
/// tail (length not a multiple of 4) is silently dropped by `chunks_exact`
/// rather than erroring — the stored BLOB is written by this same codebase
/// and never expected to be malformed, but a decode helper has no business
/// panicking on stray bytes either way.
fn decode_le_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

/// The stored embedding for `message_id`, decoded, if one exists. Shared by
/// both `embed_message_ad_hoc` (checked BEFORE building a network client, so
/// the common case truly makes no network call at all) and
/// `embed_message_ad_hoc_with_backend` (checked again so the testable core
/// function is correct when called directly, e.g. from a test).
fn stored_embedding(
    db: &Database,
    message_id: &str,
    model: &str,
) -> Result<Option<Vec<f32>>, CoreError> {
    // A vector from another model is in a different space: ignore it so the
    // anchor is re-embedded with the current model, and its neighbours (all
    // filtered to this model) are comparable.
    Ok(db
        .get_message_embedding(message_id)
        .map_err(|e| CoreError::Db(e.to_string()))?
        .filter(|rec| rec.model_name == model)
        .map(|rec| decode_le_f32(&rec.vector)))
}

/// Ad-hoc embedding computation for one message, given an already-connected
/// `backend` and `model` — the testable core of `embed_message_ad_hoc`,
/// split out the same way `run_ai_jobs_json`/`run::run_ai_jobs_with_backend`
/// split so tests can inject a mock `AiBackend`
/// (see `run::tests::MockBackend`) instead of a real network client.
///
/// (a) An existing stored embedding is returned as-is — zero backend calls.
/// (b) Otherwise the message's embeddable text is built (`run::
/// build_embed_text`, the SAME source `execute_embed_job` uses, so an
/// ad-hoc and a background-job embedding of the same message are
/// numerically identical), `backend.embed()` is called exactly ONCE
/// (never retried — a UI gesture that's actively waiting gets one attempt
/// and a fast, honest failure, unlike the durable job queue's backoff/retry
/// loop), and on success the vector is persisted via the existing
/// `upsert_message_embedding` so the next call for the same message is
/// instant.
///
/// This does NOT go through `defense::envelope`/`validate_output`: those
/// gate model-GENERATED TEXT reaching a UI surface or storage, and an
/// embedding is a numeric vector, not generated text — `execute_embed_job`
/// bypasses `defense` for the identical reason.
fn embed_message_ad_hoc_with_backend(
    db: &Database,
    backend: &mut dyn AiBackend,
    model: &str,
    message_id: &str,
) -> Result<Vec<f32>, CoreError> {
    if let Some(vector) = stored_embedding(db, message_id, model)? {
        return Ok(vector);
    }

    let text = run::build_embed_text(db, message_id)
        .map_err(|e| CoreError::Db(e.to_string()))?
        .ok_or_else(|| CoreError::Sync("message has no text to embed yet".into()))?;

    let vector =
        run::embed_capped(&*backend, model, &text).map_err(|e| CoreError::Sync(e.to_string()))?;
    if vector.is_empty() {
        return Err(CoreError::Sync(
            "backend returned an empty embedding vector".into(),
        ));
    }

    let bytes: Vec<u8> = vector.iter().flat_map(|f| f.to_le_bytes()).collect();
    db.upsert_message_embedding(message_id, model, vector.len() as i64, &bytes)
        .map_err(|e| CoreError::Db(e.to_string()))?;
    Ok(vector)
}

/// Public entry point (Assumption 4): checks for a stored embedding first
/// (no network client is even built in that case), otherwise connects a
/// real `OllamaClient` the same way `run_ai_jobs_json` does and delegates to
/// [`embed_message_ad_hoc_with_backend`]. Synchronous/blocking, with the
/// same timeout discipline (`config.request_timeout_secs`) the rest of the
/// AI seam uses — callers (the FFI boundary) are responsible for running
/// this off the Swift main thread.
pub fn embed_message_ad_hoc(
    db: &Database,
    config: &AiRunConfig,
    message_id: &str,
) -> Result<Vec<f32>, CoreError> {
    let model = config
        .embedding_model
        .clone()
        .unwrap_or_else(|| config.model.clone());
    if let Some(vector) = stored_embedding(db, message_id, &model)? {
        return Ok(vector);
    }
    let ai_config = AiConfig {
        base_url: config.endpoint.clone(),
        request_timeout_secs: config.request_timeout_secs.unwrap_or(300),
        connect_timeout_secs: 10,
    };
    let mut client =
        OllamaClient::connect(&ai_config).map_err(|e| CoreError::Sync(e.to_string()))?;
    embed_message_ad_hoc_with_backend(db, &mut client, &model, message_id)
}

#[derive(Serialize)]
struct SimilarMessageJson<'a> {
    header: &'a MessageHeader,
    similarity: f32,
}

/// "Find similar" (S53-UI): embed the anchor message (ad-hoc, reusing a
/// stored embedding when one exists), then run
/// `Database::find_similar_messages` against it, and serialize `{ results:
/// [{ header, similarity }] }` — the Profile-API-shaped JSON wrapper
/// `castellum_ffi::castellum_find_similar_messages` calls into, same
/// layering as every other `*_json` function in this module.
///
/// `account_id: None` searches every account (Assumption per
/// `Database::find_similar_messages`); `limit` is clamped to non-negative.
pub fn find_similar_json(
    db: &Database,
    config: &AiRunConfig,
    message_id: &str,
    account_id: Option<&str>,
    limit: i64,
) -> Result<String, CoreError> {
    let embedding = embed_message_ad_hoc(db, config, message_id)?;
    let model = config
        .embedding_model
        .as_deref()
        .unwrap_or(config.model.as_str());
    let results = db
        .find_similar_messages_for_model(
            account_id,
            &embedding,
            message_id,
            limit.max(0),
            Some(model),
        )
        .map_err(|e| CoreError::Db(e.to_string()))?;
    let payload: Vec<SimilarMessageJson> = results
        .iter()
        .map(|(header, similarity)| SimilarMessageJson {
            header,
            similarity: *similarity,
        })
        .collect();
    let value = serde_json::json!({ "results": payload });
    serde_json::to_string(&value)
        .map_err(|e| CoreError::Sync(format!("serialize find_similar: {e}")))
}

#[cfg(test)]
mod find_similar_tests {
    use super::*;
    use castellum_providers::AiError;
    use run::tests::{default_config, fresh_db, seed_message, MockBackend};
    use std::time::{Duration, Instant};

    #[test]
    fn embed_message_ad_hoc_never_calls_backend_when_already_embedded() {
        let db = fresh_db();
        seed_message(&db, "acc1:INBOX:1");
        db.upsert_message_embedding(
            "acc1:INBOX:1",
            "nomic-embed-text",
            3,
            &[0, 0, 128, 63, 0, 0, 0, 0, 0, 0, 0, 0], // [1.0, 0.0, 0.0] LE
        )
        .unwrap();
        // embed_result: None => MockBackend::embed panics if called at all.
        let mut backend = MockBackend::succeeds(vec![]);

        let vector = embed_message_ad_hoc_with_backend(
            &db,
            &mut backend,
            "nomic-embed-text",
            "acc1:INBOX:1",
        )
        .unwrap();
        assert_eq!(vector, vec![1.0, 0.0, 0.0]);
    }

    #[test]
    fn embed_message_ad_hoc_ignores_a_vector_from_another_model() {
        let db = fresh_db();
        seed_message(&db, "acc1:INBOX:1");
        db.upsert_message_embedding("acc1:INBOX:1", "old-model", 2, &[0, 0, 128, 63, 0, 0, 0, 0])
            .unwrap();
        let mut backend = MockBackend::embeds(vec![0.5, 0.5, 0.0]);
        let v = embed_message_ad_hoc_with_backend(&db, &mut backend, "new-model", "acc1:INBOX:1")
            .unwrap();
        assert_eq!(v, vec![0.5, 0.5, 0.0]);
        assert_eq!(
            db.get_message_embedding("acc1:INBOX:1")
                .unwrap()
                .unwrap()
                .model_name,
            "new-model"
        );
    }

    #[test]
    fn find_similar_json_only_compares_vectors_of_the_current_model() {
        let db = fresh_db();
        seed_message(&db, "acc1:INBOX:1");
        seed_message(&db, "acc1:INBOX:2");
        seed_message(&db, "acc1:INBOX:3");
        let one = [0u8, 0, 128, 63, 0, 0, 0, 0]; // [1.0, 0.0]
        db.upsert_message_embedding("acc1:INBOX:1", "m", 2, &one)
            .unwrap();
        db.upsert_message_embedding("acc1:INBOX:2", "m", 2, &one)
            .unwrap();
        // Same dimension but a different model: must not appear as a neighbour.
        db.upsert_message_embedding("acc1:INBOX:3", "other", 2, &one)
            .unwrap();
        let config = AiRunConfig {
            embedding_model: Some("m".into()),
            ..default_config()
        };
        let json = find_similar_json(&db, &config, "acc1:INBOX:1", None, 10).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let ids: Vec<&str> = v["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["header"]["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["acc1:INBOX:2"]);
    }

    #[test]
    fn embed_message_ad_hoc_calls_backend_once_then_persists() {
        let db = fresh_db();
        seed_message(&db, "acc1:INBOX:1");
        let mut backend = MockBackend::embeds(vec![0.5, 0.5, 0.0]);

        let vector = embed_message_ad_hoc_with_backend(
            &db,
            &mut backend,
            "nomic-embed-text",
            "acc1:INBOX:1",
        )
        .unwrap();
        assert_eq!(vector, vec![0.5, 0.5, 0.0]);
        assert!(
            db.get_message_embedding("acc1:INBOX:1").unwrap().is_some(),
            "a freshly ad-hoc-computed embedding is persisted"
        );

        // A second call for the same message now hits the stored row — the
        // backend would panic if `embed()` were called again, so a plain
        // success here proves zero further backend calls.
        let mut backend2 = MockBackend::succeeds(vec![]);
        let vector2 = embed_message_ad_hoc_with_backend(
            &db,
            &mut backend2,
            "nomic-embed-text",
            "acc1:INBOX:1",
        )
        .unwrap();
        assert_eq!(vector2, vec![0.5, 0.5, 0.0]);
    }

    /// The "never blocks" proof (AUTONOMOUS-2026-09-19's advisory #3): a
    /// backend that sleeps then fails must still return control to the
    /// caller within a bounded wall-clock time, never hang indefinitely,
    /// and never retry on its own (a single failed attempt is surfaced
    /// immediately — the durable job queue's backoff/retry loop is a
    /// different code path this function deliberately does not use).
    #[test]
    fn embed_message_ad_hoc_never_blocks_on_a_slow_failing_backend() {
        let db = fresh_db();
        seed_message(&db, "acc1:INBOX:1");
        let mut backend =
            MockBackend::embed_slow_fails_with(Duration::from_millis(50), || AiError::Timeout);

        let start = Instant::now();
        let result = embed_message_ad_hoc_with_backend(
            &db,
            &mut backend,
            "nomic-embed-text",
            "acc1:INBOX:1",
        );
        let elapsed = start.elapsed();

        assert!(result.is_err(), "a timing-out backend surfaces an error");
        assert!(
            elapsed < Duration::from_secs(2),
            "must return promptly, not hang indefinitely (took {elapsed:?})"
        );
    }

    #[test]
    fn find_similar_json_never_blocks_when_the_anchor_needs_embedding_and_the_backend_times_out() {
        let db = fresh_db();
        seed_message(&db, "acc1:INBOX:1");
        // find_similar_json's public entry point goes through
        // embed_message_ad_hoc (real OllamaClient), which cannot be mocked
        // here without a live endpoint — so this proves the bound via the
        // shared with_backend core instead, at the same call boundary
        // find_similar_json itself uses.
        let mut backend =
            MockBackend::embed_slow_fails_with(Duration::from_millis(50), || AiError::Timeout);
        let start = Instant::now();
        let result = embed_message_ad_hoc_with_backend(
            &db,
            &mut backend,
            "nomic-embed-text",
            "acc1:INBOX:1",
        );
        let elapsed = start.elapsed();
        assert!(result.is_err());
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }

    #[test]
    fn find_similar_json_finds_similar_messages_using_a_stored_anchor_embedding() {
        let db = fresh_db();
        seed_message(&db, "acc1:INBOX:1");
        seed_message(&db, "acc1:INBOX:2");
        db.upsert_message_embedding("acc1:INBOX:1", "m", 3, &embed_le(&[1.0, 0.0, 0.0]))
            .unwrap();
        db.upsert_message_embedding("acc1:INBOX:2", "m", 3, &embed_le(&[0.9, 0.1, 0.0]))
            .unwrap();
        let config = AiRunConfig {
            embedding_model: Some("m".into()),
            ..default_config()
        };

        let json = find_similar_json(&db, &config, "acc1:INBOX:1", Some("acc1"), 50).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let results = value["results"].as_array().unwrap();
        assert_eq!(
            results.len(),
            1,
            "the anchor never appears in its own results"
        );
        assert_eq!(results[0]["header"]["id"], "acc1:INBOX:2");
        assert!(results[0]["similarity"].as_f64().unwrap() > 0.9);
    }

    fn embed_le(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|f| f.to_le_bytes()).collect()
    }
}

// ---------------------------------------------------------------------------
// mlx feature off: selecting AiBackendKind::Mlx must fail with a clear,
// typed error rather than silently falling back to Ollama or panicking.
// Only compiled when the `mlx` feature is NOT set — with it on, this same
// selection goes through the real MlxClient path exercised by
// `run::tests::mlx_backed_generate_is_aibackend_and_delegates_embed_and_list_models`
// and friends instead. See DECISIONS.md 2026-09-25.
// ---------------------------------------------------------------------------
#[cfg(all(test, not(feature = "mlx")))]
mod mlx_feature_off_tests {
    use super::*;
    use run::tests::fresh_db;

    #[test]
    fn selecting_mlx_without_the_feature_returns_a_clear_error() {
        let db = fresh_db();
        let config_json =
            r#"{"endpoint":"http://127.0.0.1:11434","model":"llama3.1:8b","backend":"mlx"}"#;

        let err = run_ai_jobs_json(&db, config_json).expect_err(
            "selecting Mlx without the `mlx` feature must be an error, not a silent fallback",
        );

        match err {
            crate::CoreError::Sync(msg) => assert_eq!(
                msg, "MLX backend not compiled into this build",
                "the message must clearly say why, not leak backend internals"
            ),
            other => panic!("expected CoreError::Sync, got {other:?}"),
        }
    }
}
